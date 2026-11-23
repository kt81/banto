//! The "emporium" (大店): banto's one UI — a persistent left sidebar (the
//! session list, nicknamed the chōba) plus a pane area beside it that hosts the
//! staged session or a brigade's tiles, embedded.
//!
//! Per `docs/DISCIPLINE.md` §4, this module is a thin **shell**: it gathers
//! facts about the outside world into
//! [`engine::Event`]s, calls the pure [`engine::update`], and executes the
//! [`engine::Cmd`]s it returns — process spawning, PTY reads/writes, store
//! reads/writes, and drawing all live here; none of the *decisions* do (see
//! `super::engine`, which owns `Stage`/`Focus`/the relay engine/etc.).
//!
//! The pieces this draws on live outside it: `App` (list state), the `view`
//! renderers and `render_modal` (`banto-tui`), and the store-load helpers
//! (`crate::session`).
//!
//! `BANTO_RECORD_EVENTS=<path>` (see [`EventRecorder`]'s doc for what it
//! captures and why it must never be committed) captures every `Event` fed
//! into [`engine::update`] as a `docs/DISCIPLINE.md` §8 replay stream.

use std::cell::RefCell;
use std::cell::RefCell as CellRef;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{self, Stdout, Write as _};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use banto_core::app::{App, Mode};
use banto_core::config::{AgentBinaries, BrigadeConfig, KeysConfig, ResolvedAgents};
use banto_core::engine::{
    self, Axis, BellCause, BrigadeNameFacts, ButtonKind, Cmd, EmporiumState, Event, Focus, Ghost,
    GoinkyoLook, GoinkyoRow, GroupJoinTargetData, LiveStatus, PrefixKey, RelayObservation,
    SessionKey, SpawnRefusal, StoreIntent, brigade_label, decide_goinkyo,
};
#[cfg(test)]
use banto_core::engine::{GoinkyoObservation, GoinkyoSpawnCandidate};
use banto_core::input::InputEvent;
use banto_core::layout::title_text;
use banto_core::model::{
    AgentKind, Brigade, BrigadeId, BrigadeMember, BrigadeRole, DIRECTOR_TOKEN, MemberGlance,
    MemberToken, MessageGlance, Quiescence, SessionId, SessionToOpen,
};
use banto_core::pane_log as pane_lines;
use banto_core::replay::{STREAM_VERSION, TimedEvent};
use banto_core::screen::{SPAWN_COLS, SPAWN_ROWS, Screen};
use banto_core::status::AgeThresholds;
use banto_io::claude_briefing::{self, BriefingPattern};
use banto_io::claude_home::ClaudeHome;
use banto_io::codex_activity;
use banto_io::codex_catalog::{CodexCatalogSource, CodexCli};
use banto_io::codex_home::CodexHome;
use banto_io::codex_liveness::SysinfoStartTime;
use banto_io::data_dir::DataDir;
use banto_io::file_identity::DirectorySpellings;
use banto_io::input::{CrosstermInputReader, InputReader, StampedEvent};
use banto_io::pane_log::PaneLog;
use banto_io::provider::SessionProvider;
use banto_io::provider::cache::MetaCache;
use banto_io::provider::claude_code::ClaudeCodeProvider;
use banto_io::provider::codex::CodexProvider;
use banto_io::pty::{PortablePtyHost, PtyHost, STRIPPED_CHILD_ENV_VARS};
use banto_io::status::{
    LiveSession, ProcessProbe, SysinfoProbe, ancestry_distance, claude_nudgeable,
    claude_quiescence, live_activity, live_reports_busy, read_live_sessions, waiting_on_a_dialog,
};
use banto_io::store::{SCHEMA_VERSION, Store};
use banto_tui::paint;
use banto_tui::strip::{StripData, StripMessage, render_strip, render_title_status};
use banto_tui::view::{self, WAITING_ACTIVITY_COLOR};

use crate::launch;
use crate::session;
use banto_io::watch::LiveWatch;

use super::convert;
use super::paste_accum::{PasteAccumulator, is_in_scope};
use super::session::{PtyHandle, PtyPoll, wait_for_exit_or_deadline};

type Tui = Terminal<TimedBackend<CrosstermBackend<FrameWriter<Stdout>>>>;

#[derive(Clone, Copy, Default)]
struct MethodTimings {
    calls: u64,
    elapsed: Duration,
}

impl MethodTimings {
    fn record(&mut self, elapsed: Duration) {
        self.calls += 1;
        self.elapsed += elapsed;
    }
}

#[derive(Default)]
struct BackendTimings {
    draw: MethodTimings,
    append_lines: MethodTimings,
    hide_cursor: MethodTimings,
    show_cursor: MethodTimings,
    get_cursor_position: MethodTimings,
    set_cursor_position: MethodTimings,
    clear: MethodTimings,
    clear_region: MethodTimings,
    size: MethodTimings,
    window_size: MethodTimings,
    flush: MethodTimings,
    present_write: MethodTimings,
    present_flush: MethodTimings,
    bytes_written: u64,
}

/// Accumulates one presentation so the screen diff, cursor update, and OSC 8
/// close reach the host in their required order with one real write/flush.
///
/// `ratatui-crossterm` writes each cursor command through `execute!`, which
/// flushes its writer. On a Windows console that made a small cursor update as
/// expensive as the whole frame. Backend `flush` is therefore only a frame
/// boundary here; [`FrameWriter::present`] is the sole host flush.
struct FrameWriter<W> {
    output: Rc<CellRef<FrameOutput<W>>>,
    timings: Rc<CellRef<BackendTimings>>,
}

struct FrameOutput<W> {
    inner: W,
    pending: Vec<u8>,
}

/// The shell-side half of [`FrameWriter`]. Crossterm intentionally keeps its
/// writer private, so this shared handle is how the frame boundary appends
/// OSC 8 after Ratatui's cursor commands without opening a second stdout.
struct FrameOutputHandle<W> {
    output: Rc<CellRef<FrameOutput<W>>>,
    timings: Rc<CellRef<BackendTimings>>,
}

impl<W> FrameWriter<W> {
    fn new(inner: W, timings: Rc<CellRef<BackendTimings>>) -> (Self, FrameOutputHandle<W>) {
        let output = Rc::new(CellRef::new(FrameOutput {
            inner,
            pending: Vec::new(),
        }));
        (
            Self {
                output: output.clone(),
                timings: timings.clone(),
            },
            FrameOutputHandle { output, timings },
        )
    }
}

impl<W: io::Write> FrameOutputHandle<W> {
    fn has_pending(&self) -> bool {
        !self.output.borrow().pending.is_empty()
    }

    fn present(&mut self) -> io::Result<()> {
        let mut output = self.output.borrow_mut();
        if !output.pending.is_empty() {
            let pending = std::mem::take(&mut output.pending);
            let started = Instant::now();
            let result = output.inner.write_all(&pending);
            self.timings
                .borrow_mut()
                .present_write
                .record(started.elapsed());
            if let Err(error) = result {
                // Keep the queued bytes observable to the caller; the event
                // loop exits on this error rather than silently discarding a
                // half-presented frame.
                output.pending = pending;
                return Err(error);
            }
        }
        let started = Instant::now();
        let result = output.inner.flush();
        self.timings
            .borrow_mut()
            .present_flush
            .record(started.elapsed());
        result
    }

    fn write_hyperlink_close(&mut self) -> io::Result<()> {
        close_any_open_hyperlink(self)
    }
}

impl<W: io::Write> io::Write for FrameWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.borrow_mut().pending.extend_from_slice(buf);
        self.timings.borrow_mut().bytes_written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: io::Write> io::Write for FrameOutputHandle<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.borrow_mut().pending.extend_from_slice(buf);
        self.timings.borrow_mut().bytes_written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Pass-through ratatui backend which times every trait call. Its shared
/// counters let `size(&self)` participate without changing the backend API.
struct TimedBackend<B> {
    inner: B,
    timings: Rc<CellRef<BackendTimings>>,
    size: CellRef<Size>,
}

impl<B: Backend> TimedBackend<B> {
    fn new(inner: B, timings: Rc<CellRef<BackendTimings>>) -> Result<Self, B::Error> {
        let started = Instant::now();
        let size = inner.size();
        timings.borrow_mut().size.record(started.elapsed());
        Ok(Self {
            inner,
            timings,
            size: CellRef::new(size?),
        })
    }

    fn take_timings(&self) -> BackendTimings {
        std::mem::take(&mut *self.timings.borrow_mut())
    }

    fn cached_size(&self) -> Size {
        *self.size.borrow()
    }

    fn set_cached_size(&self, size: Size) {
        *self.size.borrow_mut() = size;
    }

    fn refresh_size(&mut self) -> Result<Size, B::Error> {
        let started = Instant::now();
        let size = self.inner.size();
        self.timings.borrow_mut().size.record(started.elapsed());
        let size = size?;
        self.set_cached_size(size);
        Ok(size)
    }
}

macro_rules! timed_backend_call {
    ($self:ident, $field:ident, $call:expr) => {{
        let started = Instant::now();
        let result = $call;
        $self.timings.borrow_mut().$field.record(started.elapsed());
        result
    }};
}

impl<B: Backend> Backend for TimedBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        timed_backend_call!(self, draw, self.inner.draw(content))
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        timed_backend_call!(self, append_lines, self.inner.append_lines(n))
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        timed_backend_call!(self, hide_cursor, self.inner.hide_cursor())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        timed_backend_call!(self, show_cursor, self.inner.show_cursor())
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        timed_backend_call!(self, get_cursor_position, self.inner.get_cursor_position())
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        timed_backend_call!(
            self,
            set_cursor_position,
            self.inner.set_cursor_position(position)
        )
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        timed_backend_call!(self, clear, self.inner.clear())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        timed_backend_call!(self, clear_region, self.inner.clear_region(clear_type))
    }

    fn size(&self) -> Result<ratatui::layout::Size, Self::Error> {
        let started = Instant::now();
        let size = self.cached_size();
        self.timings.borrow_mut().size.record(started.elapsed());
        Ok(size)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        timed_backend_call!(self, window_size, self.inner.window_size())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        timed_backend_call!(self, flush, self.inner.flush())
    }
}

/// The `config.toml`-derived settings [`run`] needs, bundled into one
/// parameter once a fourth one (`enabled_agents`) pushed the plain arg list
/// past clippy's `too_many_arguments` limit — mirrors `crate::launch::OpenContext`'s
/// role for the same problem there.
pub struct EmporiumSettings<'a> {
    /// `[brigade]`: how many fresh Workers `B` auto-spawns when forming a
    /// new brigade, the `--model` an auto-spawned Worker launches with, and
    /// whether the relay engine is enabled.
    pub brigade: &'a BrigadeConfig,
    /// `[keys]`: the tmux-style prefix chord for pane operations.
    pub keys: &'a KeysConfig,
    /// `[agent_binaries]` — see `crate::launch::agent_binary`.
    pub agent_binaries: &'a AgentBinaries,
    /// `Config.agents`, resolved.
    pub resolved_agents: &'a ResolvedAgents,
    /// Where this banto found its config, database and agent homes — handed on to
    /// every banto process it starts for a brigade member (`_mcp`, `_hook`), so
    /// they read the same ones.
    pub child: &'a crate::startup::Resolved,
    /// The banto executable, resolved once at start (`crate::exe`): what every
    /// brigade member's banto children are started from.
    pub exe: &'a crate::exe::BantoExe,
    /// What startup has to say about the database: that it is not in the data
    /// directory (`crate::startup::split_database_notice`), or that the check for
    /// another banto on it could not be made (`crate::single_instance`). Shown
    /// with the other startup notices.
    pub database_notices: Vec<String>,
}

/// Run the emporium until the user quits (`q`/Esc from the sidebar; a
/// second press while a session is not known to be idle).
pub fn run(
    claude_home: &ClaudeHome,
    codex_home: Option<&CodexHome>,
    thresholds: &AgeThresholds,
    store: &RefCell<Store>,
    settings: &EmporiumSettings,
) -> Result<()> {
    let brigade = settings.brigade;
    let keys = settings.keys;
    let agent_binaries = settings.agent_binaries;
    let enabled_agents = &settings.resolved_agents.enabled;
    // Janitor: purge brigades with no members left (legacy pre-v7 data, or
    // residue from a crash mid-formation) before the sidebar's brigade-
    // derived caches (hidden Workers, Directors) load. Silent by design — an
    // empty brigade is never user-visible, so there's nothing to report.
    let _ = store.borrow_mut().delete_empty_brigades();

    // Created once here so the bootstrap walk's parses are already in it when
    // the loop's first `gather_reload` runs a second later.
    let meta_cache = MetaCache::new();
    let mut metas =
        session::discover_all(claude_home, codex_home, enabled_agents, Some(&meta_cache))?;
    // The store-backed state the sidebar shows: grouping, pins, archived-hiding
    // and brigade hiding. This
    // one-time bootstrap stays outside `update`: `App::with_*` are
    // construction-only builders, not a repeating decision.
    let (rows, pinned, groups, session_groups, hidden, directors) = {
        let store = store.borrow();
        // Same two calls as `gather_reload`, same reason for the order.
        session::apply_consultation_titles(&mut metas, &store);
        let rows = session::rows_from_metas(metas, claude_home, codex_home, thresholds);
        // Before the archive marks, for the reason `gather_reload` gives.
        let hidden = crate::session::load_hidden_member_ids(&store);
        let rows = crate::session::mark_archived(rows, &store);
        let pinned = crate::session::load_pinned(&store);
        let groups = crate::session::load_groups(&store);
        let session_groups = crate::session::load_session_groups(&store, &groups);
        let directors = crate::session::load_directors(&store);
        (rows, pinned, groups, session_groups, hidden, directors)
    };
    let mut app = App::new(rows)
        .with_pinned(pinned)
        .with_groups(groups, session_groups)
        .with_hidden_member_ids(hidden)
        .with_directors(directors)
        .with_enabled_agents(enabled_agents.clone());
    // Both are shown when both apply, the trust notice first: an untrusted hook
    // is a cell about to form with members nothing will ever brief, while an
    // unknown agent name is a typo in a setting.
    let trust_notice = codex_home
        .filter(|_| enabled_agents.contains(&AgentKind::Codex))
        .and_then(|home| {
            let has_brigade = !store
                .borrow()
                .list_brigades()
                .unwrap_or_default()
                .is_empty();
            let hook_launchable = hook_launchable(settings.exe);
            session::codex_trust_notice(
                banto_io::codex_trust::hook_trust_state(home),
                true,
                has_brigade,
                hook_launchable,
            )
        });
    // The executable's is told with the database's: both are about what banto
    // stands on, decided before the first frame.
    let startup_notices = session::startup_notices(
        trust_notice,
        settings
            .database_notices
            .iter()
            .cloned()
            .chain(settings.exe.watch_notice())
            .collect(),
    );

    let deps = Deps {
        claude_home,
        codex_home,
        thresholds,
        store,
        meta_cache: &meta_cache,
        pty_host: &PortablePtyHost,
        input_reader: &CrosstermInputReader,
        brigade,
        agent_binaries,
        enabled_agents,
        codex_catalog: &CodexCli,
        child: Some(settings.child),
        exe: settings.exe,
    };
    let (mut terminal, mut frame_output) = setup_terminal()?;
    let result = event_loop(
        &mut terminal,
        &mut frame_output,
        &mut app,
        &deps,
        keys,
        startup_notices,
    );
    let restored = restore_terminal();
    result.and(restored)
}

/// A session awaiting id discovery: its shell-only bookkeeping (cwd, launch
/// time, and which brigade member it is, if any) — the core only ever sees
/// the *result* (`Event::DiscoveryResult`), never `since`/`cwd` themselves
/// (see `super::engine`'s module doc: a `SystemTime` has no meaningful
/// translation into the core's `Instant`-only clock).
struct DiscoveryTracker {
    key: SessionKey,
    cwd: PathBuf,
    /// `cwd`'s spellings, measured once at spawn — the directory is reachable
    /// then, and every poll after compares the paths agents report against
    /// them as text (see [`DirectorySpellings`]).
    spawned: DirectorySpellings,
    since: SystemTime,
    member: Option<(BrigadeId, MemberToken)>,
    /// The spawned child's pid, when the platform reports one — the exact
    /// match against `sessions/<pid>.json` that [`poll_discovery`] tries
    /// before falling back to scanning session files by cwd.
    pid: Option<u32>,
    codex_log_scan: Option<banto_io::codex_liveness::CodexLogScan>,
    /// Which product this is — Claude's own two sources above don't apply
    /// to a Codex child at all (see [`poll_discovery`]'s doc for the
    /// store-based fallback this gates).
    agent: AgentKind,
    /// Whether [`poll_discovery`] has already told the operator this Claude
    /// tracker looks stuck behind an unanswered directory-trust prompt —
    /// set the first time `claude_directory_trust_in` reports `NotTrusted`, so
    /// the status line states it once instead of restamping the same notice
    /// every poll (~every loop iteration, not just once a second). The
    /// kickoffs' own once-only bit (`PendingKickoff::notified_not_at_prompt`)
    /// lives in `engine.rs` instead, because that one belongs to the core's
    /// own typing decision; this one only ever gates a notice, and this
    /// tracker — already the shell's own discovery bookkeeping, never seen by
    /// the core (see this struct's own doc) — is exactly where that one bit
    /// belongs.
    notified_untrusted: bool,
}

/// How often the relay engine (and the pending-submit flush / status expiry
/// bundled into the same [`Event::Tick`]) re-evaluates.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Read-only shell dependencies that don't change across an [`event_loop`]
/// iteration — bundled so `event_loop`/`execute_cmd`/`gather_reload`'s argument
/// lists don't keep growing one-by-one as more reload-path state gets
/// threaded through.
struct Deps<'a> {
    claude_home: &'a ClaudeHome,
    /// `None` degrades to no Codex sessions in the sidebar, not an error.
    codex_home: Option<&'a CodexHome>,
    thresholds: &'a AgeThresholds,
    store: &'a RefCell<Store>,
    /// Transcript parses carried between reloads (`banto_io::provider::cache`).
    /// Measured here: the watcher fires with as little as half a second
    /// between reloads, almost always because a session banto does not own
    /// wrote a line to its transcript, while barely one file per interval is
    /// one whose parse could have changed.
    meta_cache: &'a MetaCache,
    /// What actually starts a child. `&PortablePtyHost` in a real run; a
    /// mock in a test, which is the whole point of it being here.
    ///
    /// It used to be named at each call site instead, and that made the two
    /// lines that open a pane unreachable by any test in this repository —
    /// including the argument that strips `CLAUDE_CODE_CHILD_SESSION`, which
    /// `cf519a7` had to establish by re-measuring a live `claude` because
    /// nothing else could see it. Every `Cmd::OpenEmbedded` the core
    /// produces, from any of its producers, funnels through one of those
    /// calls.
    pty_host: &'a dyn PtyHost,
    /// What actually reads terminal input, off its own thread — see
    /// [`banto_io::input`]'s module doc for why that thread exists.
    /// `&CrosstermInputReader` in a real run; a mock in a test, same role
    /// [`Self::pty_host`] plays for spawning.
    input_reader: &'a dyn InputReader,
    /// `[brigade]` from config.toml. Lives here rather than as its own
    /// `event_loop` parameter because `execute_cmd` needs it too now (a
    /// member's launch argv carries its role briefing — see
    /// [`member_briefing`]).
    brigade: &'a BrigadeConfig,
    /// `[agent_binaries]` from config.toml — see `launch::agent_binary`.
    agent_binaries: &'a AgentBinaries,
    /// `Config.agents`, resolved.
    enabled_agents: &'a BTreeSet<AgentKind>,
    /// What reads the installed Codex's model catalog: `&CodexCli` in a real
    /// run, a mock in a test — an external process like the PTY host, so no
    /// test of the emporium runs Codex.
    codex_catalog: &'a dyn CodexCatalogSource,
    /// What a brigade member's banto children are told to resolve (see
    /// [`EmporiumSettings::child`]); `None` in a test, which then builds the
    /// argv and environment a hand-started child would get.
    child: Option<&'a crate::startup::Resolved>,
    /// The executable those children are started from, as it was at start; see
    /// [`crate::exe::BantoExe::check`] for what a member open asks of it.
    exe: &'a crate::exe::BantoExe,
}

fn event_loop(
    terminal: &mut Tui,
    frame_output: &mut FrameOutputHandle<Stdout>,
    app: &mut App,
    deps: &Deps,
    keys: &KeysConfig,
    startup_notices: Vec<String>,
) -> Result<()> {
    let brigade = deps.brigade;
    let mut state = EmporiumState::new(PrefixKey::parse(&keys.prefix));
    let mut handles: HashMap<SessionKey, PtyHandle> = HashMap::new();
    let mut discovery: Vec<DiscoveryTracker> = Vec::new();
    let mut watch = LiveWatch::new(
        deps.claude_home,
        deps.codex_home
            .filter(|_| deps.enabled_agents.contains(&AgentKind::Codex)),
    );
    let provider = ClaudeCodeProvider::new(deps.claude_home.clone());
    let mut last_tick: Option<Instant> = None;
    let mut glance_gate = GlanceGate::default();
    let mut session_moves = SessionMoveWatch::default();
    // The stored sidebar width, fed as the very first event of the very first
    // iteration (see the loop) so it is part of the recorded stream.
    let mut stored_sidebar_width = Some(Event::SidebarWidthLoaded {
        width: deps.store.borrow().sidebar_width().ok().flatten(),
    });
    // The bell switch is a preference of the same kind, loaded the same way; a
    // missing or unreadable value is off.
    let mut stored_bell_switch = Some(Event::BellWhenFocusedLoaded {
        on: deps
            .store
            .borrow()
            .bell_when_focused()
            .ok()
            .flatten()
            .unwrap_or(false),
    });
    // The agent the Worker picker was last confirmed with, the same way; none
    // stored, or a value this build cannot read, is "none".
    let mut stored_worker_agent = Some(Event::LastWorkerAgentLoaded {
        agent: deps.store.borrow().last_worker_agent().ok().flatten(),
    });
    // Fed right after it, for the same reason: as part of the recorded stream.
    let mut startup_notices = (!startup_notices.is_empty()).then_some(Event::StartupNotices {
        notices: startup_notices,
    });
    // Opened once, off this thread — see `banto_io::input`'s module doc for
    // why the read no longer happens inline here: a slow iteration
    // (rendering, a reload pass, ...) between this loop getting back around
    // to receiving and an event's true arrival must never inflate
    // `paste_acc`'s gap timing.
    let input_rx = deps.input_reader.open();
    let mut input_log = open_input_log();
    let mut stage_log = open_stage_log();
    let mut bell_log = open_bell_log();
    let mut pane_log = open_pane_log(deps);
    let mut paste_acc = PasteAccumulator::new();
    let run_start = Instant::now();
    let mut event_recorder = open_event_recorder(run_start);
    let mut pane_render_cache: HashMap<SessionKey, PaneRenderCache> = HashMap::new();
    let mut terminal_size = terminal.backend().cached_size();
    let mut presented: Option<PresentedFrame> = None;

    loop {
        // Start a new diagnostic sample before this iteration's work.
        let _ = terminal.backend().take_timings();
        let now = Instant::now();
        let mut events: VecDeque<Event> = VecDeque::new();
        // Ahead of the first `Resized` and of anything an operator could do:
        // the stage is empty until a later iteration, so no pane exists whose
        // size could be computed with the default width first. (A native
        // resize arriving in this same iteration is moved to the front of
        // `events` below; the viewport height it sets does not depend on the
        // sidebar's width, so the order between the two does not matter.)
        events.extend(stored_sidebar_width.take());
        events.extend(stored_bell_switch.take());
        events.extend(stored_worker_agent.take());
        events.extend(startup_notices.take());

        // Geometry is cached from startup, native resize input, and the
        // once-a-second fallback below. `Terminal::draw` used to perform a
        // second actual console-size query after this one every iteration.
        let size = terminal.backend().cached_size();
        if (size.width, size.height) != state.size {
            events.push_back(Event::Resized {
                width: size.width,
                height: size.height,
            });
        }

        // Pump every registered handle (kept-alive sessions off-stage keep
        // advancing too, matching the pre-migration "pump every session").
        // Lost output is reported separately; the handle stays until the
        // waiter reports that the child actually exited.
        let pty_pump_start = Instant::now();
        pump_pty_handles(&mut handles, &mut events);
        let pty_pump_dur = pty_pump_start.elapsed();

        // One real input event, waited for on the reader thread's channel
        // with a short timeout (the pacing knob for the whole loop, matching
        // the pre-migration cadence) — dropped from 50ms to 10ms while
        // `paste_acc` holds buffered keys, so a lone buffered key still
        // flushes within one `PASTE_GAP` of arriving instead of waiting on
        // the ordinary idle cadence (see `paste_accum`'s module doc).
        // Converted from crossterm at this boundary (`convert::from_crossterm`)
        // — `None` for an event kind banto ignores (a key release, ...),
        // which simply contributes nothing to this tick.
        let poll_timeout = if paste_acc.is_pending() {
            Duration::from_millis(10)
        } else {
            Duration::from_millis(50)
        };
        match read_or_idle_tick(&input_rx, poll_timeout, &mut paste_acc)? {
            InputStep::Received(StampedEvent {
                event: raw,
                at: event_now,
            }) => {
                log_input(&mut input_log, &describe_raw_event(&raw));
                // Intercepted before `convert::from_crossterm`, not routed
                // through it: banto's own OS focus is not the same thing as
                // which pane holds focus inside it (`engine::Focus`), but a
                // child that asked for DECSET 1004 cannot tell those apart —
                // which is exactly why both have to resolve through one path
                // (`Event::WindowFocusChanged`'s own doc). Neither is a key, so
                // neither may reach `is_in_scope`'s paste-accumulation gate.
                if let crossterm::event::Event::Resize(width, height) = raw {
                    terminal.backend().set_cached_size(Size::new(width, height));
                    // The former per-frame size query put resize ahead of
                    // PTY/input events; retain that ordering now that the
                    // input thread supplies the prompt notification. Replace
                    // the loop's older cached observation too: startup can
                    // otherwise queue 80x24, receive 120x40 before dispatch,
                    // and incorrectly apply the stale size last.
                    events.retain(|event| !matches!(event, Event::Resized { .. }));
                    events.push_front(Event::Resized { width, height });
                } else if let Some(ev) = window_focus_event(&raw) {
                    events.push_back(ev);
                } else if let Some(input) = convert::from_crossterm(raw) {
                    log_input(&mut input_log, &describe_converted_event(&input));
                    if is_in_scope(&state, app, &input) {
                        // A stale buffer (idle past `PASTE_GAP` before this key
                        // arrived) flushes first, so it never silently merges
                        // into this key's run.
                        if let Some(flushed) = paste_acc.tick(event_now) {
                            emit_flushed(&mut events, &mut input_log, flushed);
                        }
                        let InputEvent::Key(key) = input else {
                            unreachable!("is_in_scope only admits InputEvent::Key")
                        };
                        if let Some(flushed) = paste_acc.accept(key, event_now) {
                            emit_flushed(&mut events, &mut input_log, flushed);
                        }
                    } else {
                        // Out of scope for accumulation: flush whatever is
                        // buffered first (preserving key order), except a mouse
                        // event, which passes through with the buffer left
                        // untouched (see `paste_accum::PasteAccumulator::bypass`).
                        let is_mouse = matches!(input, InputEvent::Mouse(_));
                        if let Some(flushed) = paste_acc.bypass(is_mouse) {
                            emit_flushed(&mut events, &mut input_log, flushed);
                        }
                        events.push_back(Event::Input(input));
                    }
                }
            }
            InputStep::Idle(flushed) => {
                if let Some(flushed) = flushed {
                    emit_flushed(&mut events, &mut input_log, flushed);
                }
            }
        }

        // Discovery: poll for the ids Claude assigns to freshly-launched
        // sessions still awaiting one. `claimed` — the ids already spoken
        // for — is read off the handle map, which is only truthful because
        // `Cmd::RekeyPty` renames a handle the moment its id is discovered:
        // without that, a resolved id stays invisible here and the next
        // pending tracker resolves onto it a second time.
        let discovery_start = Instant::now();
        if !discovery.is_empty() {
            let claimed: HashSet<String> =
                handles.keys().map(|key| key.as_str().to_string()).collect();
            // Only a Claude tracker looks at the live files, and this runs every
            // pass; a Codex-only pending set would pay the directory read for nothing.
            let live = if discovery
                .iter()
                .any(|tracker| tracker.agent == AgentKind::ClaudeCode)
            {
                read_live_sessions(&deps.claude_home.sessions_dir())
            } else {
                Vec::new()
            };
            events.extend(poll_discovery(
                &mut discovery,
                &provider,
                &claimed,
                &live,
                deps.store,
                deps.claude_home,
            ));
            if last_tick.is_none_or(|tick| now.duration_since(tick) >= TICK_INTERVAL)
                && let Some(home) = deps.codex_home
            {
                events.extend(poll_solo_codex_discovery(
                    &mut discovery,
                    home,
                    &claimed,
                    &SysinfoStartTime,
                ));
            }
        }
        let discovery_dur = discovery_start.elapsed();

        let reload_start = Instant::now();
        if watch.poll_ready(SystemTime::now()) {
            events.extend(gather_reload(deps));
        }
        let reload_dur = reload_start.elapsed();

        // ~1s: relay observations for every live brigade pane, gathered here
        // (store + live-session reads) and decided in `update` — plus the
        // trigger for the pending-submit flush and status expiry bundled
        // into the same tick (see `engine::update_tick`'s doc).
        let tick_start = Instant::now();
        let mut tick_timings = TickTimings::default();
        let mut goinkyo_owed = false;
        if last_tick.is_none_or(|tick| now.duration_since(tick) >= TICK_INTERVAL) {
            let first_tick = last_tick.is_none();
            last_tick = Some(now);
            // Startup already made the one authoritative query that seeded
            // the cache. The initial application tick still runs at once,
            // but the fallback poll begins a full interval later.
            if !first_tick {
                let size = terminal.backend_mut().refresh_size()?;
                if (size.width, size.height) != state.size {
                    events.retain(|event| !matches!(event, Event::Resized { .. }));
                    events.push_front(Event::Resized {
                        width: size.width,
                        height: size.height,
                    });
                }
            }
            let tick_read = TickRead::read(deps.store, deps.claude_home, Some(&mut tick_timings));
            let alive = |pid: u32| tick_read.alive_claude().contains(&pid);
            events.extend(gather_session_moves(
                &tick_read,
                &state,
                &handles,
                app,
                &mut session_moves,
                &MoveSources {
                    processes: &ProcessView {
                        probe: &SysinfoProbe,
                        alive: &alive,
                    },
                    owns_thread: &|thread, pid| {
                        deps.codex_home.is_some_and(|home| {
                            banto_io::codex_liveness::is_thread_owned_by(
                                home,
                                thread,
                                pid,
                                &banto_io::codex_liveness::SysinfoStartTime,
                            )
                        })
                    },
                },
                Some(&mut tick_timings),
            ));
            let mut relay = gather_relay_observations_from(
                &tick_read,
                &state,
                deps.store,
                deps.codex_home,
                app,
                Some(&mut tick_timings),
                &mut glance_gate,
            );
            note_briefed_since_launch(&mut relay, &tick_read, &handles);
            let brigades = std::mem::take(&mut glance_gate.brigades);
            // Ahead of the tick: the core decides the brigade-quiet bell on
            // the sample, but only in the tick that follows it (the same
            // instant), after the relay has decided what to type. Sent
            // every tick, even when the roster could not be read: the core
            // replaces its last sample with it, so nothing it was told earlier
            // outlives a tick that could not confirm it.
            let samples = std::mem::take(&mut glance_gate.quiescence);
            events.push_back(Event::QuiescenceSampled { samples });
            events.push_back(Event::Tick { relay, brigades });
            goinkyo_owed = true;
        }
        let tick_dur = tick_start.elapsed();

        // Drain the event queue through `update`, executing the `Cmd`s it
        // returns and feeding any follow-up fact back onto the same queue —
        // the synchronous relaxation (DISCIPLINE §6.1) generalized from just
        // the store to every shell-executed `Cmd` (spawning a PTY child is
        // just as synchronous a call as a store write).
        let dispatch_start = Instant::now();
        // The Goinkyo looks are read inside this loop (`next_event`) but belong
        // to the tick's cost, as they did when they were read above it, so
        // `slow tick` blames a slow store read on the stage that made it.
        let mut goinkyo_dur = Duration::ZERO;
        while let Some(ev) = next_event(&mut events, &mut goinkyo_owed, || {
            let observe_start = Instant::now();
            let observed = goinkyo_events(
                &state,
                deps.store,
                app,
                pane_log.as_mut(),
                Some(&mut tick_timings),
            );
            goinkyo_dur += observe_start.elapsed();
            observed
        }) {
            if let Some(recorder) = &mut event_recorder {
                recorder.record(&ev, now);
            }
            // Named before the event is consumed, and only when the log will use it.
            let produced_by = bell_log.is_some().then(|| variant_name(&ev));
            // Read before `update` folds the event: the facts about the pane
            // the log names are the ones the event found, not the ones it leaves.
            let by = pane_lines::event_name(&ev);
            if let Some(log) = pane_log.as_mut() {
                let facts = pane_facts_for(&state, app, &handles, &ev, now);
                log.emit_all(&pane_lines::event_lines(&ev, &facts));
            }
            let cmds = engine::update(&mut state, app, brigade, ev, now);
            for cmd in cmds {
                if let Cmd::RingHostBell { cause } = &cmd {
                    let entry = bell_log.as_mut().map(|file| BellEntry {
                        file,
                        since_start: now.saturating_duration_since(run_start),
                        event: produced_by.as_deref().unwrap_or(""),
                    });
                    ring_host_bell_to(&mut io::stdout(), cause, entry);
                    continue;
                }
                // Cloned before `execute_cmd` consumes the Cmd, and only for the
                // kinds the log records. `handle_present` is read before it runs
                // for the same reason: the handle may be gone after.
                let source =
                    (pane_log.is_some() && pane_lines::is_logged_cmd(&cmd)).then(|| cmd.clone());
                let handle_present =
                    pane_lines::cmd_handle_key(&cmd).is_some_and(|key| handles.contains_key(key));
                let results = execute_cmd(cmd, deps, &mut handles, &mut discovery);
                if let (Some(log), Some(source)) = (pane_log.as_mut(), source) {
                    // An open replaces a handle under its key only when it succeeds,
                    // and a success is the one event an open produces.
                    let replaced = handle_present
                        && results
                            .iter()
                            .any(|event| matches!(event, Event::Spawned { .. }));
                    // Read after the open, since the handle it left is what has a pid.
                    let pid = pane_lines::cmd_handle_key(&source)
                        .and_then(|key| handles.get(key))
                        .and_then(PtyHandle::pid);
                    let facts = pane_lines::CmdFacts {
                        handle_present,
                        replaced,
                        pid,
                    };
                    if let Some(line) = pane_lines::cmd_line(&source, by, facts) {
                        log.emit(&line);
                    }
                }
                events.extend(results);
            }
        }
        let tick_dur = tick_dur + goinkyo_dur;
        let dispatch_dur = dispatch_start.elapsed().saturating_sub(goinkyo_dur);

        // A `PtyExited` or `PtyOutputLost` handler drops the session's `Screen`;
        // a lost-output handle stays until its child actually exits.
        // This is also why every core-side `screens` rekey must reach the
        // handle map (`Cmd::RekeyPty`): a handle left under a stale key
        // reads as "screen gone" and is reaped here, which on Unix closes
        // the PTY master and SIGHUPs a perfectly live child.
        //
        // The reaped handles are dropped on a thread of their own, and that
        // is not tidiness. Closing a pseudoconsole can block until its
        // output has been drained and its client has exited, and by the time
        // a handle is dropped its reader thread is already gone — so the
        // close waits for a drain that will never happen. Dropping inline
        // froze the whole UI after `prefix x`: rendering had completed, the
        // process sat at zero CPU, and no further input was ever read.
        // Whatever the exact contract turns out to be, a teardown that can
        // block must not run on the thread that serves the operator.
        let reaped = reap_pty_handles(&mut handles, &state);
        if let Some(log) = pane_log.as_mut() {
            for (key, handle) in &reaped {
                log.emit(&pane_lines::reaped_line(key, handle.pid()));
            }
        }
        drop_trackers_of_gone_panes(&mut discovery, &state);
        if !reaped.is_empty() {
            std::thread::spawn(move || drop(reaped));
        }

        let draw_start = Instant::now();
        let mut draw_timings = DrawTimings::default();
        if app.should_quit() {
            // The frame about to be drawn is the one the operator watches while
            // the sweep below runs. A comparison against the previous frame
            // could in principle find nothing new to send; this one is always
            // sent.
            presented = None;
        }
        let cached_size = terminal.backend().cached_size();
        if cached_size != terminal_size {
            terminal.resize(Rect::new(0, 0, cached_size.width, cached_size.height))?;
            terminal_size = cached_size;
        }
        if app.modal().is_some() {
            // `Frame` does not expose the cursor a modal requests. Its text
            // entry is uncommon and must remain exact, so retain Ratatui's
            // normal application path while still batching its host output.
            let assemble_start = Instant::now();
            let drawn = terminal
                .draw(|frame| {
                    let _ = draw(
                        frame,
                        app,
                        &state,
                        SystemTime::now(),
                        now,
                        &mut pane_render_cache,
                    );
                })
                .map(|_| ());
            draw_timings.assemble += assemble_start.elapsed();
            if frame_output.has_pending() {
                let hyperlink_start = Instant::now();
                frame_output.write_hyperlink_close()?;
                draw_timings.hyperlink_close = hyperlink_start.elapsed();
                draw_timings.hyperlink_bytes = b"\x1b]8;;\x1b\\".len() as u64;
            }
            let present_result = frame_output.present();
            drawn?;
            present_result?;
            // The modal's private cursor request cannot be represented in
            // `PresentedFrame`; force one normal comparison after it closes.
            presented = None;
        } else {
            let (buffer, cursor) = {
                let frame = &mut terminal.get_frame();
                let assemble_start = Instant::now();
                let cursor = draw(
                    frame,
                    app,
                    &state,
                    SystemTime::now(),
                    now,
                    &mut pane_render_cache,
                );
                draw_timings.assemble += assemble_start.elapsed();
                (frame.buffer_mut().clone(), cursor)
            };
            let changed = should_present(presented.as_ref(), &buffer, cursor);
            if changed {
                // The writer below absorbs ratatui's backend flush. That lets
                // us append the OSC 8 close after cursor placement, then do
                // exactly one real host write and flush for the presentation.
                let applied = terminal.apply_buffer_with_cursor(cursor).map(|_| ());
                if frame_output.has_pending() {
                    // Ratatui resets SGR after a frame but knows nothing of
                    // OSC 8. Its cell diff can transmit an unchanged link
                    // opener while skipping its separately painted close,
                    // leaving later prose linked. A bare close after every
                    // frame that actually wrote cells gives OSC 8 the same
                    // boundary guarantee as Ratatui's SGR reset. Skipped
                    // frames wrote no opener, so they must emit nothing.
                    let hyperlink_start = Instant::now();
                    frame_output.write_hyperlink_close()?;
                    draw_timings.hyperlink_close = hyperlink_start.elapsed();
                    draw_timings.hyperlink_bytes = b"\x1b]8;;\x1b\\".len() as u64;
                }
                // Present even if Ratatui reported an operation error, so
                // queued output is never left dangling in the frame writer.
                let present_result = frame_output.present();
                applied?;
                present_result?;
                presented = Some(PresentedFrame { buffer, cursor });
            } else {
                // Keep Ratatui's previous buffer as the next diff baseline
                // while clearing this unpresented work buffer for next time.
                terminal.current_buffer_mut().reset();
                draw_timings.skipped = true;
            }
        }
        let draw_dur = draw_start.elapsed();
        draw_timings.backend = terminal.backend().take_timings();

        log_slow_iteration(
            &mut stage_log,
            &StageTimings {
                pty_pump: pty_pump_dur,
                discovery: discovery_dur,
                reload: reload_dur,
                tick: tick_dur,
                dispatch: dispatch_dur,
                draw: draw_dur,
            },
            &tick_timings,
            &draw_timings,
        );

        if app.should_quit() {
            break;
        }
    }
    shutdown_handles(&mut handles, SHUTDOWN_GRACE, SHUTDOWN_POLL_INTERVAL);
    Ok(())
}

/// One iteration's read step, off the reader thread's channel.
#[derive(Debug)]
enum InputStep {
    /// A real event arrived — carries the reader thread's own stamp.
    Received(StampedEvent),
    /// The channel genuinely went quiet for the whole `poll_timeout`
    /// window; `Some` if that was long enough to flush a buffered run.
    Idle(Option<InputEvent>),
}

/// The channel wait plus the one wall-clock staleness check it gates,
/// pulled out of [`event_loop`] so a test can drive it against a real
/// `mpsc` channel with controlled stamps — no thread, no real waiting. See
/// the tests below for the property this exists to prove: `recv_timeout`
/// always gets first look at the channel, so a buffer is never judged stale
/// by wall clock while a fresher event is already sitting there unread
/// (drain-before-tick — the same call-order hazard `PasteAccumulator::
/// accept`'s own doc names for tick-before-accept, `paste_accum.rs`).
/// `Err(RecvTimeoutError::Timeout)` is trustworthy evidence of idleness
/// specifically *because* the channel is the drain: reaching it means
/// `recv_timeout` waited out the whole window with nothing arriving, not
/// merely "nothing seen recently."
fn read_or_idle_tick(
    input_rx: &mpsc::Receiver<StampedEvent>,
    poll_timeout: Duration,
    paste_acc: &mut PasteAccumulator,
) -> Result<InputStep> {
    match input_rx.recv_timeout(poll_timeout) {
        Ok(stamped) => Ok(InputStep::Received(stamped)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let flushed = if paste_acc.is_pending() {
                paste_acc.tick(Instant::now())
            } else {
                None
            };
            Ok(InputStep::Idle(flushed))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("input reader thread ended unexpectedly")
        }
    }
}

/// How long quit waits for the children it hung up before it kills them: the
/// time every other end banto asks for gives a pane
/// ([`banto_core::engine::PANE_END_GRACE`], which says where the five seconds
/// come from), spent once for the whole sweep (see `shutdown_handles` for why
/// it is shared, not per child).
const SHUTDOWN_GRACE: Duration = banto_core::engine::PANE_END_GRACE;
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Graceful-close-then-wait-then-force teardown for every still-registered
/// PTY child, run once the event loop above has broken out (quit), before
/// `run` calls `restore_terminal`. Each child gets `PtyHandle::
/// begin_graceful_close`'s normal console-close shutdown — so an embedded
/// `claude` finalizes its session data exactly as it would on any ordinary
/// window close — instead of an abrupt kill, bounded by one shared deadline
/// so an unresponsive child can't extend the others' wait, let alone
/// banto's own exit. Anything still alive past the deadline is force-killed
/// via the existing `Killer` and shutdown proceeds without waiting further.
///
/// `grace`/`poll_interval` are parameters (rather than reading the
/// module-level constants directly) purely so tests can drive this with a
/// short deadline instead of the real 5s.
///
/// The same shape as every other end banto asks for — hang up, give the child
/// [`banto_core::engine::PANE_END_GRACE`], then kill — except that the sweep owns
/// the waiting here, because the event loop that would time it has stopped.
/// Hanging up is done off this thread (`PtyHandle::begin_graceful_close`), so on
/// a Windows build whose console close blocks, the children are not closed one
/// after another before the shared deadline has even started.
fn shutdown_handles(
    handles: &mut HashMap<SessionKey, PtyHandle>,
    grace: Duration,
    poll_interval: Duration,
) {
    for handle in handles.values_mut() {
        handle.begin_graceful_close();
    }
    let mut pending: Vec<&mut PtyHandle> = handles.values_mut().collect();
    wait_for_exit_or_deadline(&mut pending, Instant::now() + grace, poll_interval);
    for handle in handles.values_mut() {
        if !handle.has_exited() {
            let _ = handle.kill();
        }
    }
}

/// Execute one `Cmd`, returning any follow-up `Event`(s) — the only place
/// that writes to a hosted session's stdin, spawns a process, or touches
/// the store.
fn execute_cmd(
    cmd: Cmd,
    deps: &Deps,
    handles: &mut HashMap<SessionKey, PtyHandle>,
    discovery: &mut Vec<DiscoveryTracker>,
) -> Vec<Event> {
    match cmd {
        Cmd::WritePty { key, bytes } => {
            if let Some(handle) = handles.get_mut(&key) {
                handle.send_bytes(&bytes);
                Vec::new()
            } else {
                // Reported rather than swallowed — see
                // `engine::update_pty_write_dropped`'s own doc for why this
                // is expected to mean a bug, not an ordinary race, and for
                // the exact class of bug (a stale key surviving in some
                // queued-write state past a rename) this is the backstop
                // for.
                vec![Event::PtyWriteDropped { key }]
            }
        }
        Cmd::InterruptPty { key } => {
            if let Some(handle) = handles.get_mut(&key) {
                handle.interrupt();
                Vec::new()
            } else {
                // The pane ended between the decision and this write: the same
                // backstop as for a dropped line, and nothing else to undo (the
                // outcome is already on its way to the store as `pressed`).
                vec![Event::PtyWriteDropped { key }]
            }
        }
        Cmd::ResizePty { key, rows, cols } => {
            if let Some(handle) = handles.get_mut(&key) {
                handle.resize(rows, cols);
            }
            Vec::new()
        }
        Cmd::HangUpPty { key } => {
            if let Some(handle) = handles.get_mut(&key) {
                handle.begin_graceful_close();
            }
            Vec::new()
        }
        Cmd::KillPty { key } => {
            if let Some(handle) = handles.get_mut(&key) {
                let _ = handle.kill();
            }
            Vec::new()
        }
        Cmd::RekeyPty { from, to } => {
            if let Some(handle) = handles.remove(&from) {
                handles.insert(to, handle);
            }
            Vec::new()
        }
        Cmd::CheckNewSessionCwd { cwd } => {
            let is_dir = cwd.is_dir();
            vec![Event::NewSessionCwdChecked { cwd, is_dir }]
        }
        Cmd::OpenEmbedded {
            key,
            target,
            brigade,
            model,
            effort,
            permission_mode,
            disallowed_tools,
            autocompact,
        } => execute_open_embedded(
            OpenEmbeddedRequest {
                key,
                target,
                brigade,
                model,
                effort,
                permission_mode,
                disallowed_tools,
                autocompact,
            },
            deps,
            handles,
            discovery,
        ),
        Cmd::CheckCodexTrust => execute_check_codex_trust(deps),
        Cmd::ReadCodexCatalog { round } => execute_read_codex_catalog(round, deps),
        Cmd::SweepDataDir => execute_sweep_data_dir(deps),
        Cmd::OpenCodexTrustPane { key } => execute_open_codex_trust_pane(key, deps, handles),
        Cmd::Store(intent) => execute_store_intent(intent, deps.store),
        Cmd::Reload => gather_reload(deps),
        Cmd::ForwardClipboardToHost { bytes } => {
            // Safe to write here, outside `draw`, specifically because OSC
            // 52 moves neither the cursor nor any cell — it cannot desync
            // ratatui's model of the screen the way a cursor-positioning
            // sequence would, which is the property an innocent edit could
            // break by writing something else this way. `execute_cmd` runs
            // before this iteration's frame presentation, so there is never
            // a half-written escape sequence on the wire for this to land
            // inside; `stdout()` is the same global buffered handle the
            // backend itself writes through, so the two stay serialized
            // rather than interleaved. Flushed explicitly rather than left
            // for the next presentation's flush
            // — a copy landing a frame late for no reason defeats a feature
            // whose whole point is that the operator feels it arrive.
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(&bytes).and_then(|()| stdout.flush());
            Vec::new()
        }
        Cmd::RingHostBell { cause } => {
            // The event loop rings through here too, with its log; this arm is
            // for a caller that has none.
            ring_host_bell_to(&mut std::io::stdout(), &cause, None);
            Vec::new()
        }
    }
}

/// One executed bell, as the diagnostic log wants it written down.
struct BellEntry<'a> {
    file: &'a mut std::fs::File,
    /// Time since the run started, counted from the same instant the event
    /// recorder counts `offset_ms` from, so a line here can be laid against a
    /// recording.
    since_start: Duration,
    /// Which event's update produced the bell ([`variant_name`]).
    event: &'a str,
}

/// Write the host bell and, when `log` is given, one line saying why and
/// whether the write worked. Like OSC 52 forwarding, BEL changes neither cells
/// nor cursor, so emitting it before draw cannot desynchronize ratatui's model;
/// it is deliberately a fixed byte, never child-controlled payload. A failed
/// write or log is no error: a diagnostic must never break the run, and a
/// bell nobody heard is not worth stopping for.
fn ring_host_bell_to<W: io::Write>(host: &mut W, cause: &BellCause, log: Option<BellEntry<'_>>) {
    let written = host.write_all(b"\x07").and_then(|()| host.flush());
    let Some(entry) = log else {
        return;
    };
    let wall_ms = std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let outcome = match written {
        Ok(()) => "written".to_string(),
        Err(error) => format!("failed={:?}", error.kind()),
    };
    // Ids, tokens and counts only (`BellCause`'s own contract): never a title,
    // a message or anything from a screen.
    let _ = writeln!(
        entry.file,
        "{wall_ms} emporium: bell +{}ms cause={cause} event={} {outcome}",
        entry.since_start.as_millis(),
        entry.event,
    );
}

/// The name of an event's variant. It stops the `Debug` formatting at the first
/// character that is not part of the name, so a chunk of terminal output is not
/// walked to say it is one.
fn variant_name(event: &Event) -> String {
    use std::fmt::Write as _;
    struct NameOnly(String);
    impl std::fmt::Write for NameOnly {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            for c in text.chars() {
                if !(c.is_alphanumeric() || c == '_') {
                    return Err(std::fmt::Error);
                }
                self.0.push(c);
            }
            Ok(())
        }
    }
    let mut name = NameOnly(String::new());
    let _ = write!(name, "{event:?}");
    name.0
}

/// Writes a bare OSC 8 close (`ESC ] 8 ; ; ESC \`) into the current frame
/// output. It is emitted only after a presentation that queued screen bytes;
/// a skipped frame sends nothing at all. Split out so its exact bytes and the
/// flush boundary are independently testable without a real terminal.
fn close_any_open_hyperlink<W: io::Write>(w: &mut W) -> io::Result<()> {
    w.write_all(b"\x1b]8;;\x1b\\")?;
    w.flush()
}

/// Delete the working files no member of the store needs any more. Runs here, in
/// the emporium, and nowhere else: the `_mcp` processes only ever write. What is
/// safe to delete, and what the sweep assumes, is `banto_io::data_dir`'s to say.
///
/// The authority check lives here, at the edge: only a data directory whose own
/// database is the one this process opened is swept
/// ([`crate::startup::Resolved::sweepable_data_dir`]). An emporium pointed at
/// another database shares the directory but not the store, and would delete the
/// everyday one's live files. A store that cannot be read deletes nothing.
fn execute_sweep_data_dir(deps: &Deps) -> Vec<Event> {
    if let Some(dir) = deps.child.and_then(|child| child.sweepable_data_dir()) {
        sweep_data_dir_with(&dir, deps.store, std::time::SystemTime::now());
    }
    Vec::new()
}

fn sweep_data_dir_with(dir: &DataDir, store: &RefCell<Store>, now: std::time::SystemTime) {
    banto_io::data_dir::sweep(dir, now, || {
        let rows = store.borrow().all_member_tokens().ok()?;
        Some(banto_io::data_dir::Facts::from_member_rows(rows))
    });
}

/// Whether the hook command could carry the executable's path (no space in it),
/// by the path resolved at start. An unknowable path counts as launchable:
/// blaming a space nothing has established is there would be worse than leaving
/// the trust question to speak for itself.
fn hook_launchable(exe: &crate::exe::BantoExe) -> bool {
    exe.path()
        .is_none_or(|exe| launch::hook_command_is_launchable(&launch::forward_slash_path(exe)))
}

/// Read whether banto's `SessionStart` hook looks trusted right now —
/// freshly, not cached, since the operator may have approved it in a pane
/// since this run started — and whether it could even fire from this
/// executable's path. `deps.codex_home` absent (Codex unresolved) reads as
/// unprimed: `Cmd::CheckCodexTrust` is only ever issued once a brigade
/// formation already resolved to a Codex Director or Codex Workers, so
/// "can't tell" must not read as "trusted".
fn execute_check_codex_trust(deps: &Deps) -> Vec<Event> {
    let primed = deps.codex_home.is_some_and(|home| {
        banto_io::codex_trust::hook_trust_state(home)
            == banto_io::codex_trust::HookTrustState::Primed
    });
    let hook_launchable = hook_launchable(deps.exe);
    vec![Event::CodexTrustChecked {
        primed,
        hook_launchable,
    }]
}

/// Ask the installed Codex which models it ships with
/// (`banto_io::codex_catalog`), through the same binary a launch would use
/// (`[agent_binaries]` or `codex` on the path). Whatever comes back, or why
/// nothing did, goes to the core as it is: the choice of model is its to make
/// (`banto_core::codex_model`).
fn execute_read_codex_catalog(round: u64, deps: &Deps) -> Vec<Event> {
    let binary = launch::agent_binary(AgentKind::Codex, deps.agent_binaries);
    vec![Event::CodexCatalogRead {
        round,
        result: deps.codex_catalog.bundled_catalog(&binary),
    }]
}

/// Spawn Codex's own trust-review startup (`crate::codex_trust::trust_argv`
/// — the exact same argv a real brigade launch's hook override would carry,
/// see that function's own doc) under `key`, staged as a solo pane
/// (`PendingOpen::Solo`, inserted by `confirm_codex_trust_modal` before this
/// Cmd was even issued). Deliberately not routed through
/// `execute_open_embedded`/discovery: this is a throwaway review session,
/// not one banto should ever track or show in the sidebar.
fn execute_open_codex_trust_pane(
    key: SessionKey,
    deps: &Deps,
    handles: &mut HashMap<SessionKey, PtyHandle>,
) -> Vec<Event> {
    let exe = match deps.exe.check() {
        Ok(exe) => exe,
        Err(error) => {
            return vec![Event::SpawnFailed {
                key,
                error,
                refusal: SpawnRefusal::ExeUnavailable,
                os_error: None,
            }];
        }
    };
    let argv = crate::codex_trust::trust_argv(exe, deps.agent_binaries);
    let cwd = std::env::current_dir().ok();
    match PtyHandle::open(
        deps.pty_host,
        &argv,
        cwd.as_deref(),
        &[],
        STRIPPED_CHILD_ENV_VARS,
        SPAWN_ROWS,
        SPAWN_COLS,
    ) {
        Ok(handle) => {
            handles.insert(key.clone(), handle);
            vec![Event::Spawned {
                key,
                agent: Some(AgentKind::Codex),
            }]
        }
        Err(err) => vec![Event::SpawnFailed {
            key,
            error: err.to_string(),
            refusal: SpawnRefusal::PtyOpen,
            os_error: os_error_of(&err),
        }],
    }
}

/// Build the [`launch::AgentLaunch`] for opening `target`: resuming it when a
/// real id is already known (`None` if [`launch::is_live`] finds it already
/// running somewhere, so a resume would fork its history), or creating a
/// session otherwise.
///
/// `model` and `effort` reach the argv only for a creation. A resume drops them
/// here whatever the caller passed (`launch::SessionStart`'s doc says why: the
/// product restores what the session last used, and a flag would overwrite a
/// choice the operator made inside it), so the rule holds even for a producer
/// that forgets it. The [`RoleFlags`] and `briefing` are the role's, and ride on
/// both.
/// `target.agent` picks the variant; `briefing` only ever reaches the
/// `Claude` one, because only Claude takes a briefing as launch argv; a
/// Codex member gets the same text from the `banto _hook` process the
/// injected `SessionStart` hook spawns
/// (docs/notes/codex-briefing-spike.md).
///
/// `briefing` is the member's already-rendered role briefing
/// (`--append-system-prompt`, see [`crate::briefing::render`]) — rendered by
/// the caller because it needs a store read for the roster. Passing it does not
/// by itself put it in front of a resumed session's model: Claude reuses the
/// system prompt it recorded on the conversation's first request, so on a resume
/// the caller adds `--system-prompt-snapshot off` when the recorded copy is not
/// this text ([`resume_needs_snapshot_off`]).
///
/// Pulled out of [`execute_open_embedded`] so this decision logic is
/// unit-testable without spawning a real PTY; the returned launch's
/// `mcp_config` (Claude) and `brigade` (Codex) are left `None` here and
/// filled in by the caller, since both need real I/O this doesn't — a config
/// file write and the running executable's own path.
fn build_open_launch(
    target: &SessionToOpen,
    model: Option<&str>,
    effort: Option<&str>,
    flags: RoleFlags<'_>,
    briefing: Option<&str>,
    ctx: &launch::OpenContext,
) -> Option<launch::AgentLaunch> {
    let start = if target.id.is_empty() {
        launch::SessionStart::Create {
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
        }
    } else {
        // The no-double-resume guard (AGENTS.md invariant 4): a session that
        // is already running somewhere else — another banto, or an agent
        // started by hand — is never resumed a second time.
        if launch::is_live(target, ctx) {
            return None;
        }
        launch::SessionStart::Resume(target.id.clone())
    };
    Some(match target.agent {
        AgentKind::ClaudeCode => launch::AgentLaunch::Claude {
            start,
            permission_mode: flags.permission_mode.map(str::to_string),
            disallowed_tools: flags.disallowed_tools.map(str::to_string),
            autocompact: flags.autocompact.map(str::to_string),
            append_system_prompt: briefing.map(str::to_string),
            mcp_config: None,
        },
        // `briefing` is deliberately unused here: Codex has no
        // `--append-system-prompt`, so a Codex member's briefing is rendered
        // by the `banto _hook` process the injected SessionStart hook spawns,
        // not passed on the argv. `brigade` is left `None` for the caller to
        // fill for the same reason `mcp_config` is — it needs the running
        // executable's own path, which is I/O this stays free of.
        // `permission_mode` and `disallowed_tools` are dropped:
        // `AgentLaunch::Codex` has no field for either (Claude-only flags),
        // and a Codex Worker does arrive with `permission_mode` set.
        // `autocompact` is not: it arrives already in Codex's own terms (a
        // token count, or `None` for `auto`).
        AgentKind::Codex => launch::AgentLaunch::Codex {
            start,
            cwd: target.cwd.clone(),
            autocompact: flags.autocompact.map(str::to_string),
            brigade: None,
        },
    })
}

/// The flags a launch carries because of the role, not the session: the part of
/// [`Cmd::OpenEmbedded`] that rides on a resume as it does on a creation. Claude
/// takes all of them; a Codex launch takes only `autocompact`
/// ([`launch::AgentLaunch::Codex`] has no field for the other two), so a Codex
/// target drops those here whatever it is handed.
/// Bundled so [`build_open_launch`] stays under clippy's `too_many_arguments`.
#[derive(Default)]
struct RoleFlags<'a> {
    permission_mode: Option<&'a str>,
    disallowed_tools: Option<&'a str>,
    autocompact: Option<&'a str>,
}

/// The per-call parameters of [`Cmd::OpenEmbedded`], bundled so
/// [`execute_open_embedded`] takes one request plus shared process state
/// (`deps`/`handles`/`discovery`) rather than five separate fields —
/// keeps it under clippy's `too_many_arguments`, which the `effort` field
/// pushed past on its own.
struct OpenEmbeddedRequest {
    key: SessionKey,
    target: SessionToOpen,
    brigade: Option<(BrigadeId, MemberToken, BrigadeRole)>,
    model: Option<String>,
    effort: Option<String>,
    permission_mode: Option<String>,
    disallowed_tools: Option<String>,
    autocompact: Option<String>,
}

/// Spawn `target` under `key`, enforcing the no-double-resume guard
/// ([`launch::is_live`]) for a known (non-empty) id, or skipping it entirely for
/// a fresh (empty-id) spawn, which has no existing session to double-resume. `brigade` wires the launch to banto's own MCP
/// server; a write failure there now refuses the spawn outright
/// (`Event::SpawnFailed`) rather than the pre-migration behavior of
/// spawning anyway, without the flag. That silent degrade was measured live:
/// a member launches, reads its briefing off the argv/hook same as always,
/// and then simply has no `send_to_peer`/`check_messages` — indistinguishable
/// on screen from a session that is merely quiet. A pane an operator can see
/// is worse than no pane at all when it looks alive but cannot actually act
/// as a brigade member.
fn execute_open_embedded(
    request: OpenEmbeddedRequest,
    deps: &Deps,
    handles: &mut HashMap<SessionKey, PtyHandle>,
    discovery: &mut Vec<DiscoveryTracker>,
) -> Vec<Event> {
    let OpenEmbeddedRequest {
        key,
        target,
        brigade,
        model,
        effort,
        permission_mode,
        disallowed_tools,
        autocompact,
    } = request;
    if target.agent == AgentKind::Codex
        && !target.id.is_empty()
        && discovery
            .iter()
            .any(|tracker| tracker.agent == AgentKind::Codex)
    {
        return vec![Event::SpawnFailed {
            key,
            error: "waiting to identify a new Codex pane before resuming".to_string(),
            refusal: SpawnRefusal::CodexIdentifying,
            os_error: None,
        }];
    }
    let claude_home = deps.claude_home;
    // Only read live sessions when a resume might actually need them — a
    // fresh (unresumed) spawn never does.
    let live = if target.id.is_empty() {
        Vec::new()
    } else {
        read_live_sessions(&claude_home.sessions_dir())
    };
    let briefing = brigade
        .as_ref()
        .and_then(|(brigade_id, token, role)| member_briefing(deps, *brigade_id, token, *role));
    let open_ctx = launch::OpenContext {
        probe: &SysinfoProbe,
        live: &live,
        codex_home: deps.codex_home,
        start_time: &SysinfoStartTime,
    };
    let Some(mut launch) = build_open_launch(
        &target,
        model.as_deref(),
        effort.as_deref(),
        RoleFlags {
            permission_mode: permission_mode.as_deref(),
            disallowed_tools: disallowed_tools.as_deref(),
            autocompact: autocompact.as_deref(),
        },
        briefing.as_deref(),
        &open_ctx,
    ) else {
        return vec![Event::SpawnFailed {
            key,
            error: "already running elsewhere".to_string(),
            refusal: SpawnRefusal::AlreadyRunning,
            os_error: None,
        }];
    };
    // Each product reaches banto's own MCP server a different way: Claude via
    // a config file named on the argv, Codex via `-c` overrides built from
    // banto's executable path. Either can fail (a full disk, a moved
    // executable, ...) — refused outright rather than spawned wireless (see
    // this function's own doc for why).
    if let Some((brigade_id, token, role)) = &brigade {
        // This pane's agent is about to start banto children from the path
        // resolved at start. A file that is no longer the one this UI runs from
        // would make them another build: refused here, not found out by the agent.
        let exe = match deps.exe.check() {
            Ok(exe) => exe,
            Err(error) => {
                return vec![Event::SpawnFailed {
                    key,
                    error,
                    refusal: SpawnRefusal::ExeUnavailable,
                    os_error: None,
                }];
            }
        };
        let known_id = (!target.id.is_empty()).then_some(target.id.as_str());
        let child_args = deps
            .child
            .map(|child| child.child_args())
            .unwrap_or_default();
        let wiring = match &mut launch {
            launch::AgentLaunch::Claude { mcp_config, .. } => data_dir_of(deps)
                .ok_or_else(|| anyhow::anyhow!("could not determine banto's data directory"))
                .and_then(|data_dir| {
                    write_mcp_config(
                        &data_dir,
                        exe,
                        *brigade_id,
                        token,
                        *role,
                        known_id,
                        &child_args,
                    )
                })
                .map(|path| {
                    *mcp_config = Some(path);
                }),
            launch::AgentLaunch::Codex { brigade: slot, .. } => {
                *slot = Some(launch::CodexBrigade {
                    exe: exe.to_path_buf(),
                    brigade_id: *brigade_id,
                    token: token.clone(),
                    role: *role,
                    session: known_id.map(str::to_string),
                    child_args: child_args.clone(),
                });
                Ok(())
            }
        };
        if let Err(err) = wiring {
            return vec![Event::SpawnFailed {
                key,
                error: format!("brigade channel wiring failed: {err}"),
                refusal: SpawnRefusal::ChannelWiring,
                os_error: os_error_of(&err),
            }];
        }
    }
    let snapshot_off =
        resume_needs_snapshot_off(deps, &target, brigade.as_ref(), briefing.as_deref());
    let argv = launch.argv_with_snapshot_off(
        &launch::agent_binary(target.agent, deps.agent_binaries),
        snapshot_off,
    );
    let env = pane_env(target.agent, brigade.as_ref(), deps.child);
    let discovery_since = SystemTime::now();
    let mut codex_log_scan = (target.agent == AgentKind::Codex && target.id.is_empty())
        .then(|| {
            deps.codex_home
                .and_then(banto_io::codex_liveness::CodexLogScan::open)
        })
        .flatten();
    // Size is corrected on this same tick's resize pass, once staged.
    match PtyHandle::open(
        deps.pty_host,
        &argv,
        Some(&target.cwd),
        &env,
        STRIPPED_CHILD_ENV_VARS,
        SPAWN_ROWS,
        SPAWN_COLS,
    ) {
        Ok(handle) => {
            let pid = handle.pid();
            if let (Some(scan), Some(pid)) = (&mut codex_log_scan, pid) {
                scan.bind_pid(pid, &SysinfoStartTime);
            }
            if let Some((brigade_id, token, _)) = brigade.as_ref() {
                // The pane and its member survive a missed ledger update; a
                // later open retries it, and until then relay falls back to
                // the App row exactly as it did before durable agent kinds.
                let _ = deps
                    .store
                    .borrow_mut()
                    .set_member_agent(*brigade_id, token, target.agent);
            }
            handles.insert(key.clone(), handle);
            if key.is_synthetic() {
                discovery.push(DiscoveryTracker {
                    key: key.clone(),
                    agent: target.agent,
                    spawned: DirectorySpellings::of(&target.cwd),
                    cwd: target.cwd,
                    since: discovery_since,
                    member: brigade.map(|(brigade_id, token, _)| (brigade_id, token)),
                    pid,
                    codex_log_scan,
                    notified_untrusted: false,
                });
            }
            vec![Event::Spawned {
                key,
                agent: Some(target.agent),
            }]
        }
        Err(err) => vec![Event::SpawnFailed {
            key,
            error: err.to_string(),
            refusal: SpawnRefusal::PtyOpen,
            os_error: os_error_of(&err),
        }],
    }
}

fn execute_store_intent(intent: StoreIntent, store: &RefCell<Store>) -> Vec<Event> {
    match intent {
        StoreIntent::SetPin { id, pinned } => {
            let store = store.borrow();
            let session_id = SessionId(id);
            let _ = if pinned {
                store.pin(&session_id)
            } else {
                store.unpin(&session_id)
            };
            Vec::new()
        }
        StoreIntent::Archive { id, title } => {
            let result = store
                .borrow()
                .archive_session(&SessionId(id))
                .map_err(|err| err.to_string());
            vec![Event::ArchiveDone { title, result }]
        }
        StoreIntent::Unarchive { id, title } => {
            let result = store
                .borrow()
                .unarchive_session(&SessionId(id))
                .map_err(|err| err.to_string());
            vec![Event::UnarchiveDone { title, result }]
        }
        StoreIntent::JoinGroup { session_id, target } => {
            let mut store = store.borrow_mut();
            let result = match target {
                GroupJoinTargetData::Existing(group_id, name) => store
                    .set_session_group(&SessionId(session_id.clone()), group_id)
                    .map(|()| Some((group_id, name)))
                    .map_err(|err| err.to_string()),
                GroupJoinTargetData::New(name) => match store.create_group(&name) {
                    Ok(group_id) => store
                        .set_session_group(&SessionId(session_id.clone()), group_id)
                        .map(|()| Some((group_id, name.clone())))
                        .map_err(|err| err.to_string()),
                    Err(err) => Err(format!("failed to create group \"{name}\": {err}")),
                },
                GroupJoinTargetData::Leave => store
                    .clear_session_group(&SessionId(session_id.clone()))
                    .map(|()| None)
                    .map_err(|err| err.to_string()),
            };
            vec![Event::GroupJoinDone { session_id, result }]
        }
        StoreIntent::RenameGroup {
            group_id,
            old_name,
            new_name,
        } => {
            let result = store
                .borrow()
                .rename_group(group_id, &new_name)
                .map_err(|err| err.to_string());
            vec![Event::GroupRenameDone {
                group_id,
                old_name,
                new_name,
                result,
            }]
        }
        StoreIntent::DeleteGroup { group_id, name } => {
            let result = store
                .borrow_mut()
                .delete_group(group_id)
                .map_err(|err| err.to_string());
            vec![Event::GroupDeleteDone {
                group_id,
                name,
                result,
            }]
        }
        StoreIntent::ResolveMembership { session_id } => {
            let store = store.borrow();
            let membership = store
                .brigade_of_session(&SessionId(session_id.clone()))
                .ok()
                .flatten();
            // In the order they joined: it is what the tiles of a brigade the
            // operator never rearranged are stacked in.
            let roster = membership.as_ref().map(|(brigade_id, _, _)| {
                store
                    .brigade_members_in_join_order(*brigade_id)
                    .unwrap_or_default()
            });
            // The members' display names (only the named ones, as the store
            // reads them) ride the same roster, so a staging shows them at once.
            let names: Vec<(MemberToken, String)> = roster
                .iter()
                .flatten()
                .filter_map(|member| Some((member.token.clone(), member.display_name.clone()?)))
                .collect();
            let members = membership.as_ref().and(roster).map(|roster| {
                roster
                    .into_iter()
                    .map(|member| (member.token, member.role, member.session_id.map(|id| id.0)))
                    .collect()
            });
            // The brigade's stored arrangement rides the same round trip, so
            // it is in core state before the stage (and its first PTY sizes)
            // is built. A failed read is no arrangement, not a failed resolve.
            let layout = membership
                .as_ref()
                .and_then(|(brigade_id, _, _)| store.brigade_layout(*brigade_id).ok());
            vec![Event::MembershipResolved {
                session_id,
                membership,
                members,
                layout,
                names,
            }]
        }
        // Preferences are written the way a pin is: fire and forget, nothing
        // reported on failure. The arrangement is memory-first and rewritten in
        // full on its next change, so a failed write costs at most one restart's
        // worth of arrangement, not a session.
        StoreIntent::SaveBrigadeLayout { brigade_id, layout } => {
            let _ = store.borrow_mut().save_brigade_layout(brigade_id, &layout);
            Vec::new()
        }
        StoreIntent::SaveSidebarWidth { width } => {
            let _ = store.borrow().set_sidebar_width(width);
            Vec::new()
        }
        StoreIntent::SaveBellWhenFocused { on } => {
            let _ = store.borrow().set_bell_when_focused(on);
            Vec::new()
        }
        // Conditional on the sequence, so an answer to an older request never
        // wipes a newer one; a failed write is silent, and the core has already
        // forgotten the request and will not take it back.
        StoreIntent::ClearRequest { brigade_id, seq } => {
            let _ = store.borrow_mut().clear_request(brigade_id, seq);
            Vec::new()
        }
        // The one writer of the outcome column (the `_mcp` process cannot see a
        // screen). A write that changes nothing is a repeated or late one and is
        // meant to; a failed write leaves the request undecided in the store, and
        // the tool answers "queued, not decided yet" and `brigade_status` shows
        // nothing for it.
        StoreIntent::RecordInterruptOutcome {
            message_id,
            outcome,
            ..
        } => {
            let _ = store
                .borrow_mut()
                .record_interrupt_outcome(message_id, outcome);
            Vec::new()
        }
        // A failed write loses a preference, not the brigade.
        StoreIntent::SaveLastWorkerAgent { agent } => {
            let _ = store.borrow().set_last_worker_agent(agent);
            Vec::new()
        }
        StoreIntent::FormBrigade {
            director_row_id,
            director_agent,
            name,
            cwd,
            worker_count,
            worker_agent,
            worker_model,
            worker_effort,
        } => {
            let result = form_brigade_store(
                store,
                &director_row_id,
                director_agent,
                &name,
                worker_count,
                worker_agent,
            );
            vec![Event::BrigadeFormed {
                director_row_id,
                name,
                cwd,
                worker_agent,
                worker_model,
                worker_effort,
                result,
            }]
        }
        StoreIntent::AddWorker {
            brigade_id,
            cwd,
            worker_agent,
            worker_model,
            worker_effort,
        } => {
            let result = add_worker_store(store, brigade_id, worker_agent);
            vec![Event::WorkerAdded {
                brigade_id,
                cwd,
                worker_agent,
                worker_model,
                worker_effort,
                result,
            }]
        }
        StoreIntent::Disband { brigade_id } => {
            let mut store = store.borrow_mut();
            let result = store
                .delete_brigade(brigade_id)
                .map_err(|err| err.to_string())
                .map(|()| {
                    (
                        crate::session::load_hidden_member_ids(&store),
                        crate::session::load_directors(&store),
                    )
                });
            vec![Event::Disbanded { brigade_id, result }]
        }
        StoreIntent::DismissWorker { brigade_id, token } => {
            let mut store = store.borrow_mut();
            let result = store
                .dismiss_worker(brigade_id, &token)
                .map_err(|err| err.to_string())
                .map(|_| {
                    (
                        crate::session::load_hidden_member_ids(&store),
                        crate::session::load_directors(&store),
                    )
                });
            vec![Event::WorkerDismissed { brigade_id, result }]
        }
        // The same store operation for a Worker that ended on its own and could
        // not be relaunched; what differs is the answer, which names the member
        // and says whether a row was there.
        StoreIntent::DismissExitedWorker {
            brigade_id,
            token,
            repeated,
        } => {
            let mut store = store.borrow_mut();
            let result = store
                .dismiss_worker(brigade_id, &token)
                .map_err(|err| err.to_string())
                .map(|removed| {
                    (
                        removed,
                        crate::session::load_hidden_member_ids(&store),
                        crate::session::load_directors(&store),
                    )
                });
            vec![Event::ExitedWorkerDismissed {
                brigade_id,
                token,
                repeated,
                result,
            }]
        }
        StoreIntent::SetMemberSession {
            brigade_id,
            token,
            session_id,
        } => {
            let mut store = store.borrow_mut();
            let _ = store.set_member_session(brigade_id, &token, &SessionId(session_id));
            vec![Event::MemberSessionRecorded {
                hidden: crate::session::load_hidden_member_ids(&store),
                directors: crate::session::load_directors(&store),
            }]
        }
        StoreIntent::ClearMemberSession { brigade_id, token } => {
            let mut store = store.borrow_mut();
            let _ = store.clear_member_session(brigade_id, &token);
            // Same refresh `SetMemberSession` answers with: this member's id
            // just left (or, if it had none, stayed out of) the hidden set,
            // so the sidebar's own filter needs to catch up either way.
            vec![Event::MemberSessionRecorded {
                hidden: crate::session::load_hidden_member_ids(&store),
                directors: crate::session::load_directors(&store),
            }]
        }
    }
}

/// Create the brigade, its Director row, and `worker_count` Worker rows
/// (schema v7). The brigade and its Director are one transaction
/// ([`Store::create_brigade_with_director`]): the startup janitor deletes
/// brigades with no member, so a commit between the two would leave one for it
/// to delete. The Workers are not part of it — each `add_worker` commits
/// on its own — so a failure partway through the Worker loop leaves the
/// brigade, Director, and however many Worker rows already succeeded in place;
/// this only stops issuing the remaining ones and reports the failure, rather
/// than silently skipping past it and reporting a partial success as if it
/// were complete. Rare enough (SQLite serializes writers and `Store::open`'s 5s
/// busy_timeout absorbs ordinary contention between this store's several
/// writer processes — a mid-loop failure here means that timeout ran out) that
/// reconciling the partial state isn't worth the complexity.
fn form_brigade_store(
    store: &RefCell<Store>,
    director_row_id: &str,
    director_agent: AgentKind,
    name: &str,
    worker_count: usize,
    worker_agent: AgentKind,
) -> Result<(BrigadeId, Vec<MemberToken>), String> {
    let mut store = store.borrow_mut();
    let brigade_id = store
        .create_brigade_with_director(
            name,
            DIRECTOR_TOKEN,
            &SessionId(director_row_id.to_string()),
            director_agent,
        )
        .map_err(|err| err.to_string())?;
    // Numbered by the brigade's own counter, like every later Worker: N at
    // formation leaves it at N.
    let mut tokens = Vec::new();
    for _ in 0..worker_count {
        let token = store
            .add_worker(brigade_id, Some(worker_agent))
            .map_err(|err| err.to_string())?;
        tokens.push(token);
    }
    Ok((brigade_id, tokens))
}

/// Add one more Worker to an already-formed brigade, under the next `worker-N`
/// token: the store numbers it (`Store::add_worker`, one transaction), from a
/// per-brigade counter of the highest number ever issued. Never a count of
/// current Workers and never "highest current + 1" computed here: a Worker
/// token is not reused within a brigade, so dismissing the highest-numbered
/// Worker (or all of them) does not hand its number to the next one.
fn add_worker_store(
    store: &RefCell<Store>,
    brigade_id: BrigadeId,
    worker_agent: AgentKind,
) -> Result<MemberToken, String> {
    store
        .borrow_mut()
        .add_worker(brigade_id, Some(worker_agent))
        .map_err(|err| err.to_string())
}

/// Reload the session list from disk. A read failure is tolerated (yields no
/// event, keeping the previous rows) rather than erroring the whole loop out
/// over a transient filesystem hiccup.
fn gather_reload(deps: &Deps) -> Vec<Event> {
    let Ok(mut metas) = session::discover_all(
        deps.claude_home,
        deps.codex_home,
        deps.enabled_agents,
        Some(deps.meta_cache),
    ) else {
        return Vec::new();
    };
    let store = deps.store.borrow();
    // Names are applied to the metas, which still know whether a title was the
    // operator's own (`SessionMeta::title_is_custom`); a row no longer does. This
    // read is before the hidden ids and the archive marks below, so a Goinkyo
    // dismissed in between is archived and not yet named for this load, never
    // named and not archived; the next reload has both.
    session::apply_consultation_titles(&mut metas, &store);
    let rows = session::rows_from_metas(metas, deps.claude_home, deps.codex_home, deps.thresholds);
    // The hidden member ids come BEFORE the archive marks, and an innocent
    // reorder would reintroduce a race. The Director's `_mcp` process ends a
    // Goinkyo with one transaction that deletes its member row and archives its
    // session (`Store::dismiss_worker`), so a reader sees both or neither, but
    // these are two separate reads on a store in rollback-journal mode, each
    // seeing whatever is committed when it runs. Hidden first: a dismissal that
    // lands between them leaves the id hidden *and* archived (stale, mended by
    // the next refresh), never the other combination. Marks first would let it
    // land in between and yield an id no longer hidden but not marked, listed
    // as an ordinary row, with nothing asking for another reload.
    let hidden = crate::session::load_hidden_member_ids(&store);
    let rows = crate::session::mark_archived(rows, &store);
    let directors = crate::session::load_directors(&store);
    vec![Event::RowsLoaded {
        rows,
        hidden,
        directors,
    }]
}

/// Drop the trackers whose pane no longer has a screen. With no tracker
/// giving up on a clock, this and resolving are the only ways one ends: a
/// pane that exits, is closed or is dismissed still unidentified would
/// otherwise keep its tracker — and, through the guard in
/// [`execute_open_embedded`], the refusal of every Codex resume — for the rest
/// of the run.
fn drop_trackers_of_gone_panes(trackers: &mut Vec<DiscoveryTracker>, state: &EmporiumState) {
    trackers.retain(|tracker| state.screens.contains_key(&tracker.key));
}

fn poll_solo_codex_discovery(
    trackers: &mut Vec<DiscoveryTracker>,
    home: &CodexHome,
    claimed: &HashSet<String>,
    start_time: &dyn banto_io::codex_liveness::ProcessStartTime,
) -> Vec<Event> {
    if !banto_io::codex_liveness::attribution_db_readable(&home.threads_db_path()) {
        return Vec::new();
    }
    let provider = CodexProvider::new(home.clone());
    let mut used = claimed.clone();
    let mut events = Vec::new();
    trackers.retain_mut(|tracker| {
        if tracker.agent != AgentKind::Codex || tracker.member.is_some() {
            return true;
        }
        let Some(pid) = tracker.pid else { return true };
        let Ok(since) = tracker.since.duration_since(SystemTime::UNIX_EPOCH) else {
            return true;
        };
        let candidates = provider
            .find_new_sessions_in(&tracker.spawned, tracker.since)
            .into_iter()
            .map(|id| id.0)
            .collect::<Vec<_>>();
        let Some(id) = tracker
            .codex_log_scan
            .as_mut()
            .and_then(|scan| scan.poll(home, &candidates, pid, since.as_secs(), start_time))
        else {
            return true;
        };
        if !used.insert(id.clone()) {
            return true;
        }
        events.push(Event::DiscoveryResult {
            key: tracker.key.clone(),
            session_id: id,
            member: None,
        });
        false
    });
    events
}

/// This member's `briefed_session_id`, if the store has one — the id
/// `banto _hook` recorded on stdin the last time it ran for this member
/// (`BrigadeMember::briefed_session_id`'s own doc, in `banto-core`). The
/// only discovery source that exists at all for a Codex Worker: unlike
/// Claude, nothing Codex writes to disk names a session before its first
/// turn (measured 2026-08-02 — no live-state file, no session file, no
/// `threads` row, nothing), so this is that same fact reaching the store
/// instead, once `crate::engine`'s kickoff mechanism has forced that first
/// turn to happen.
///
/// Only a briefing recorded at or after `since`, the moment this pane's child
/// was spawned. Nothing clears the column, so a member that was resumed or
/// re-staged still carries the id of its previous process; taking that one
/// identified the fresh pane as the old session on the first poll, before the
/// new process had taken a turn, and the pane was then rekeyed to the old id
/// with its kickoff dropped as acknowledged. A row with no `briefed_at` cannot
/// show it is newer, so it is not used either.
fn codex_briefed_session_id(
    store: &RefCell<Store>,
    brigade_id: BrigadeId,
    token: &str,
    since: SystemTime,
) -> Option<String> {
    store
        .borrow()
        .brigade_members(brigade_id)
        .ok()?
        .into_iter()
        .find(|member| member.token == token)
        .filter(|member| member.briefed_at.is_some_and(|at| at >= since))?
        .briefed_session_id
        .map(|session_id| session_id.0)
}

/// Whether `token`'s row records a briefing made at or after `spawned_at`, the
/// moment its pane's process was spawned; `None` when `members` does not hold
/// the token. A row with no `briefed_at` is not briefed: the column is only
/// ever written by the hook, and nothing clears it, so an older value is a
/// previous process's. The same comparison as [`codex_briefed_session_id`].
fn briefed_since_launch(
    members: &[BrigadeMember],
    token: &str,
    spawned_at: SystemTime,
) -> Option<bool> {
    let member = members.iter().find(|member| member.token == token)?;
    Some(member.briefed_at.is_some_and(|at| at >= spawned_at))
}

/// Fill [`RelayObservation::briefed_since_launch`] for every Codex member this
/// tick observed, from the roster the tick already read and the moment each
/// pane's process was spawned. Left `None` where either is missing — a roster
/// that could not be read, a member with no hosted process — which the core
/// reads as having nothing to say. Not done in the gather itself, which is
/// handed no pane handles.
fn note_briefed_since_launch(
    relay: &mut [RelayObservation],
    read: &TickRead,
    handles: &HashMap<SessionKey, PtyHandle>,
) {
    let Some(roster) = read.brigades.as_deref() else {
        return;
    };
    for observation in relay {
        if observation.agent != Some(AgentKind::Codex) {
            continue;
        }
        let (Some(handle), Some((_, Some(members)))) = (
            handles.get(&observation.key),
            roster
                .iter()
                .find(|(brigade, _)| brigade.id == observation.brigade_id),
        ) else {
            continue;
        };
        observation.briefed_since_launch =
            briefed_since_launch(members, &observation.token, handle.spawned_at());
    }
}

/// Poll every pending discovery tracker for the id its child was assigned.
///
/// Three sources, tried in that order — the first two Claude-only, mirrored
/// from the pre-migration code:
///
/// 1. **The live-state file** `sessions/<pid>.json`, matched on the child's
///    own pid. `claude` writes it at startup, and banto knows the pid it
///    spawned, so this is exact — no cwd heuristics, no batch ambiguity. It
///    is also the only source that works at all for a Worker nobody has
///    typed into yet: a session's `.jsonl` doesn't appear until its first
///    *turn*, so a brigade Worker sitting at its prompt is invisible to
///    source 2 indefinitely — and, being unidentified, it can never be
///    relay-nudged into taking that first turn either. The deadlock this
///    breaks was live: every re-stage of the cell then respawned that
///    Worker as a brand-new session.
/// 2. **Session files** (`find_new_sessions`, not the single-best
///    `find_new_session`), greedily assigned oldest-first, disambiguating a
///    batch spawned into one cwd at once. The fallback whenever the direct
///    child isn't the `claude` process itself (see `PtyIo::pid`).
/// 3. **The store's `briefed_session_id`** ([`codex_briefed_session_id`]),
///    tried only for a tracker whose `agent` is `AgentKind::Codex`: sources
///    1 and 2 don't apply to it at all — see that function's own doc. Only a
///    briefing recorded since the tracker's spawn counts.
///
/// No tracker gives up: each lives until it resolves or its pane's screen is
/// gone (the loop prunes trackers against `state.screens`). A Codex Worker's
/// used to be dropped 90 s after spawn — even when the kickoff had never been
/// typed because its pane was still not at its prompt — so a first turn taken
/// later (banto's retry, or one the operator typed) fired the hook into a store
/// nobody was reading, and the pane stayed unidentified, with
/// `unresolved_codex` set, until it exited. Whether the kickoff was typed at
/// all is the core's to report (`PendingKickoff`'s attempts), not this poll's.
///
/// A Claude tracker still unresolved (regardless of how long) whose own cwd
/// reads as definitively [`banto_io::directory_trust::DirectoryTrust::NotTrusted`]
/// reports [`Event::ClaudeWorkerDirectoryUntrusted`] once — this pane isn't
/// silent because anything is broken, it's sitting behind an unanswered
/// trust prompt, and nothing *here* is held back by it: `poll_discovery`
/// itself never types into a pane — a Worker's own first turn is always the
/// operator's, and a Goinkyo's kickoff decides for itself, from its live file
/// or its painted input line, whether the pane is at its prompt (the registry
/// read here gates no keystroke anywhere) — so this report is only ever a
/// silence to explain, never an action to hold back. (Not true of banto as a
/// whole: an already-*discovered* Claude Worker's pane does get typed into, by
/// the relay nudge — but this paragraph is about a tracker that never got that
/// far.)
///
/// `Unknown` does not fire it: that just means no record exists yet, which is
/// the ordinary shape of "never opened this directory before" and would
/// fire on nearly every fresh Worker for the second or so before its first
/// launch even finishes settling, a false positive that costs real annoyance
/// with nothing gained. `NotTrusted` alone is not a guess at "maybe still
/// pending": a real `~/.claude.json` on this machine has entries recording
/// `hasTrustDialogAccepted: false` independent of any `true` one, so a
/// directory sitting at an unanswered prompt is expected to read as
/// `NotTrusted`, not `Unknown`, by the time this ever fires.
///
/// All three discovery sources skip ids already claimed by an open session
/// or taken earlier in this same pass.
fn poll_discovery(
    trackers: &mut Vec<DiscoveryTracker>,
    provider: &dyn SessionProvider,
    claimed: &HashSet<String>,
    live: &[LiveSession],
    store: &RefCell<Store>,
    claude_home: &ClaudeHome,
) -> Vec<Event> {
    let mut used_this_pass: HashSet<String> = HashSet::new();
    let mut resolved: Vec<(usize, String)> = Vec::new();
    let mut newly_untrusted: Vec<usize> = Vec::new();
    for (i, tracker) in trackers.iter().enumerate() {
        let by_pid = (tracker.agent == AgentKind::ClaudeCode)
            .then_some(tracker.pid)
            .flatten()
            .and_then(|pid| live_session_id(live, pid, &tracker.spawned));
        let id = by_pid
            .filter(|id| !claimed.contains(id) && !used_this_pass.contains(id))
            .or_else(|| {
                if tracker.agent != AgentKind::ClaudeCode {
                    return None;
                }
                provider
                    .find_new_sessions_in(&tracker.spawned, tracker.since)
                    .into_iter()
                    .map(|id| id.0)
                    .find(|id| !claimed.contains(id) && !used_this_pass.contains(id))
            })
            .or_else(|| {
                (tracker.agent == AgentKind::Codex)
                    .then_some(tracker.member.as_ref())
                    .flatten()
                    .and_then(|(brigade_id, token)| {
                        codex_briefed_session_id(store, *brigade_id, token, tracker.since)
                    })
                    .filter(|id| !claimed.contains(id) && !used_this_pass.contains(id))
            });
        if let Some(id) = id {
            used_this_pass.insert(id.clone());
            resolved.push((i, id));
        } else if tracker.agent == AgentKind::ClaudeCode
            && !tracker.notified_untrusted
            && banto_io::directory_trust::claude_directory_trust_in(
                claude_home,
                &tracker.cwd,
                &tracker.spawned,
            ) == banto_io::directory_trust::DirectoryTrust::NotTrusted
        {
            newly_untrusted.push(i);
        }
    }
    if resolved.is_empty() && newly_untrusted.is_empty() {
        return Vec::new();
    }
    let mut events: Vec<Event> = resolved
        .iter()
        .map(|(i, id)| Event::DiscoveryResult {
            key: trackers[*i].key.clone(),
            session_id: id.clone(),
            member: trackers[*i].member.clone(),
        })
        .collect();
    events.extend(newly_untrusted.iter().filter_map(|&i| {
        trackers[i]
            .member
            .as_ref()
            .map(|(_, token)| Event::ClaudeWorkerDirectoryUntrusted {
                token: token.clone(),
            })
    }));
    for &i in &newly_untrusted {
        trackers[i].notified_untrusted = true;
    }
    let removed_indices: HashSet<usize> = resolved.into_iter().map(|(i, _)| i).collect();
    let mut i = 0;
    trackers.retain(|_| {
        let keep = !removed_indices.contains(&i);
        i += 1;
        keep
    });
    events
}

/// The session id `claude` published for the process at `pid`, if that
/// live-state entry is really the child banto spawned into `spawned`.
///
/// The cwd check is the guard against a stale file: a `sessions/<pid>.json`
/// left behind by a session whose pid the OS has since recycled would
/// otherwise hand a Worker somebody else's session id, and banto would go
/// on to `--resume` it — the one thing it must never do twice. A mismatch
/// (or a file with no cwd recorded) simply declines, leaving the session-file
/// scan to answer.
///
/// The cwd comparison is [`DirectorySpellings::names`], not `==`: Claude
/// records the directory it resolved, so a pane started through a symlink
/// reports the target, in the case it was typed. It is text only, whatever the
/// file holds.
fn live_session_id(live: &[LiveSession], pid: u32, spawned: &DirectorySpellings) -> Option<String> {
    live.iter()
        .find(|entry| {
            entry.pid == pid
                && entry
                    .cwd
                    .as_deref()
                    .is_some_and(|reported| spawned.names(reported))
        })
        .and_then(|entry| entry.session_id.clone())
}

/// Gather this tick's relay observations for every live brigade member
/// (unseen messages, live idle/busy status) — the store + live-session reads
/// `engine::update_tick`'s decision logic needs, per member with a known
/// session id and an open pane among the currently-staged ones.
///
/// Idle/busy detection is per-product, resolved via `app.row_for_id` — never
/// guessed from the session id's own shape (a Claude Code id and a Codex
/// thread id are both UUID strings with no reliable, future-proof way to
/// tell them apart by form alone). A member whose product can't be resolved
/// this way (not yet in the loaded row list) reports `None`, same as a
/// Codex member when `codex_home` is absent: "unknown" is always the safe
/// default here, never "idle" — see [`codex_activity::is_thread_idle`]'s doc
/// for why manufacturing a false idle signal is the one outcome this must
/// never produce.
///
/// The Claude arm is [`claude_nudgeable`], which excludes `waiting` (for any
/// reason, a dialog included) as well as `busy` — see
/// [`banto_io::status::LIVE_STATUS_WAITING`]'s own doc for the measurement
/// behind it. Before this, `!= Some("busy")` alone counted a pending human
/// decision as idle, which let a relay nudge's text land in the middle of
/// that decision instead of the member's own input — the defect this fix
/// closes.
///
/// Codex has no equivalent exclusion, and cannot cheaply get one:
/// `codex_activity::is_thread_idle` reads the rollout's own `task_started`/
/// `task_complete` markers, which pair 1:1 per whole turn (that module's own
/// doc) with no marker in between for "the model is waiting on a tool
/// approval mid-turn" — a Codex member stuck at an approval prompt already
/// reads `Some(false)` here, the same as one still generating, so it cannot
/// reach the idle streak a nudge requires either way. That happens to avoid
/// this exact defect for Codex, but not because the state is detected —
/// nothing in this crate reads a Codex-side signal finer than "mid-turn or
/// not", so a human-facing wait that begins *after* `task_complete` (if one
/// exists) would be as invisible to this function as it was before.
fn gather_relay_observations_from(
    read: &TickRead,
    state: &EmporiumState,
    store: &RefCell<Store>,
    codex_home: Option<&CodexHome>,
    app: &App,
    mut timings: Option<&mut TickTimings>,
    glance_gate: &mut GlanceGate,
) -> Vec<RelayObservation> {
    // Before anything that can return: the loop forwards whatever the gate
    // holds after the call, so a tick that bails out must not leave the
    // previous tick's brigade facts behind — a brigade disbanded in between
    // would be handed to the core again, and named in a status, a second late.
    glance_gate.begin_tick();
    // A roster that cannot be read leaves nothing to observe about brigade
    // members, but still the panes' own state: what the live files and the
    // session list say about a pane does not depend on it. So the rest runs
    // with no brigades, and each hosted pane is sampled from what is left; one
    // that needed the roster to be understood reads as unknown.
    let roster_read = read.brigades.is_some();
    let brigades = read.brigades.as_deref().unwrap_or_default();
    let live = &read.live;
    // One shared liveness pass for every Claude member this tick,
    // not one per member: `SysinfoProbe::is_alive` was costing a full
    // `CreateToolhelp32Snapshot`-and-walk-every-process pass PER CALL on
    // Windows (sysinfo 0.39.6 — `ProcessesToUpdate::Some` still enumerates
    // the whole table before filtering), so a brigade with several Claude
    // members paid that multiple times a second. The set is the tick's
    // (`TickRead::alive_claude`, shared with the session-move gather, which may
    // have asked first and been charged for it), so it is never older than the
    // tick that read the live files. It names the processes that are still the
    // session that wrote their file, not merely pids in use: a leftover file
    // whose pid was recycled is left out, so it can neither stop the nudges nor
    // colour the tile.
    let claude_snapshot_start = Instant::now();
    let alive_claude = read.alive_claude();
    if let Some(t) = timings.as_deref_mut() {
        t.relay_claude_snapshot += claude_snapshot_start.elapsed();
    }
    let mut observations = Vec::new();
    // What this same pass learns about whether each hosted pane is idle, for
    // the quit confirmation and the brigade-quiet bell. It adds no read of its
    // own for a Claude pane — the live-file scan and the liveness pass above
    // are shared — and none for a Codex brigade member, whose rollout tail the
    // relay already reads every tick; only a hosted Codex pane outside any
    // brigade costs one (sqlite lookup plus rollout tail) per tick.
    let mut samples: Vec<(SessionKey, Quiescence)> = Vec::new();
    let mut sampled: HashSet<SessionKey> = HashSet::new();
    // One rowid lookup per tick: while no message has been appended, no
    // member's glance can have a new answer, so none is re-read.
    let glance_start = Instant::now();
    let newest_message = store.borrow().max_brigade_message_id().ok();
    if let Some(t) = timings.as_deref_mut() {
        t.relay_glance += glance_start.elapsed();
    }
    for (brigade, members) in brigades {
        let Some(members) = members else {
            continue;
        };
        // What names this brigade to the operator, from the roster just read:
        // no further query. A brigade whose roster could not be read is not
        // listed (core keeps what it had). The members' display names are the
        // whole roster's — a Worker with no session id yet has no observation
        // below, but its tile is on screen and its Director may already have
        // named it.
        // The Director's open request, one row read. A brigade whose request cannot
        // be read is left out of this tick's facts like one whose roster cannot,
        // and the core keeps what it had: an unreadable store must neither blank
        // a request the operator has not answered nor invent one.
        let request = match store.borrow().brigade_request(brigade.id) {
            Ok(request) => request.map(|seq| banto_core::engine::RequestFact { seq }),
            Err(_) => continue,
        };
        glance_gate.brigades.push(BrigadeNameFacts {
            brigade_id: brigade.id,
            name: brigade.name.clone(),
            director_session: members
                .iter()
                .find(|member| member.role == BrigadeRole::Director)
                .and_then(|member| member.session_id.as_ref())
                .map(|session| session.0.clone()),
            member_names: members
                .iter()
                .filter_map(|member| Some((member.token.clone(), member.display_name.clone()?)))
                .collect(),
            request,
            interrupts: interrupt_facts(&store.borrow(), brigade.id),
        });
        for member in members {
            let Some(session_id) = member.session_id.as_ref() else {
                continue;
            };
            let key = SessionKey::from_id(&session_id.0);
            if !state.screens.contains_key(&key) {
                continue;
            }
            let unseen_start = Instant::now();
            let has_unseen = store
                .borrow()
                .has_unseen_brigade_messages(brigade.id, &member.token, member.role)
                .unwrap_or(false);
            if let Some(t) = timings.as_deref_mut() {
                t.relay_unseen += unseen_start.elapsed();
            }
            // A fresh Claude Worker publishes live state before its first turn
            // writes the JSONL that would create an App row. Use banto's recorded
            // choice first so unread mail can break that otherwise closed loop.
            let agent = member
                .agent
                .or_else(|| app.row_for_id(&session_id.0).map(|row| row.agent));
            let is_idle_this_tick = match agent {
                Some(AgentKind::ClaudeCode) => {
                    let claude_start = Instant::now();
                    // The relay's reading of the raw status lives in one place,
                    // with the reason a `waiting` session of any kind (a dialog
                    // included) is never nudged.
                    let idle = claude_nudgeable(live, &session_id.0, alive_claude);
                    if let Some(t) = timings.as_deref_mut() {
                        t.relay_activity_claude += claude_start.elapsed();
                    }
                    idle
                }
                Some(AgentKind::Codex) => match codex_home {
                    Some(home) => {
                        let mut codex_timing = codex_activity::CodexActivityTiming::default();
                        let idle = codex_activity::is_thread_idle(
                            home,
                            &session_id.0,
                            Some(&mut codex_timing),
                        );
                        if let Some(t) = timings.as_deref_mut() {
                            t.relay_activity_codex_sqlite += codex_timing.sqlite;
                            t.relay_activity_codex_rollout += codex_timing.rollout;
                        }
                        idle
                    }
                    None => None,
                },
                None => None,
            };
            let quiescence = match agent {
                Some(AgentKind::Codex) => Quiescence::from_probe(is_idle_this_tick),
                // No product on record and no live file is `Unknown` too, so
                // an unresolved member is never read as idle.
                Some(AgentKind::ClaudeCode) | None => {
                    claude_quiescence(live, &session_id.0, alive_claude)
                }
            };
            samples.push((key.clone(), quiescence));
            sampled.insert(key.clone());
            // What the same live file says the member is doing, classified as the
            // session list classifies it, for the tile of a member that has no
            // row yet (no transcript before its first turn). Display only: the
            // relay and the watch read the file through the two functions above.
            let live_status = match agent {
                Some(AgentKind::ClaudeCode) => live_activity(live, &session_id.0, alive_claude)
                    .map(|activity| LiveStatus {
                        activity,
                        dialog_open: activity == banto_core::model::Activity::Waiting
                            && waiting_on_a_dialog(live, &session_id.0, alive_claude),
                        status_busy: live_reports_busy(live, &session_id.0, alive_claude),
                    }),
                Some(AgentKind::Codex) | None => None,
            };
            let glance_start = Instant::now();
            let glance = glance_gate.read(
                &store.borrow(),
                GlanceMember {
                    brigade_id: brigade.id,
                    token: &member.token,
                    session_id: &session_id.0,
                    role: member.role,
                },
                newest_message,
                has_unseen,
            );
            if let Some(t) = timings.as_deref_mut() {
                t.relay_glance += glance_start.elapsed();
            }
            observations.push(RelayObservation {
                brigade_id: brigade.id,
                token: member.token.clone(),
                key,
                has_unseen,
                is_idle_this_tick,
                agent,
                live: live_status,
                glance,
                briefed_since_launch: None,
            });
        }
    }
    // Every other hosted pane: a session opened on its own, or a brigade
    // member with no session id yet (a placeholder key — nothing to look up).
    for key in state.screens.keys() {
        if sampled.contains(key) {
            continue;
        }
        let quiescence = if key.is_synthetic() {
            Quiescence::Unknown
        } else {
            match app.row_for_id(key.as_str()).map(|row| row.agent) {
                Some(AgentKind::Codex) => match codex_home {
                    Some(home) => {
                        let mut codex_timing = codex_activity::CodexActivityTiming::default();
                        let idle = codex_activity::is_thread_idle(
                            home,
                            key.as_str(),
                            Some(&mut codex_timing),
                        );
                        if let Some(t) = timings.as_deref_mut() {
                            t.relay_activity_codex_sqlite += codex_timing.sqlite;
                            t.relay_activity_codex_rollout += codex_timing.rollout;
                        }
                        Quiescence::from_probe(idle)
                    }
                    None => Quiescence::Unknown,
                },
                Some(AgentKind::ClaudeCode) | None => {
                    claude_quiescence(live, key.as_str(), alive_claude)
                }
            }
        };
        samples.push((key.clone(), quiescence));
    }
    glance_gate.quiescence = samples;
    // A failed roster visited no member: ending the tick would drop every
    // remembered glance, to be read again for nothing next tick.
    if roster_read {
        glance_gate.end_tick();
    }
    observations
}

/// The next event to handle. A tick's Goinkyo observations are read here, once
/// the queue has run dry, and not when the tick's own events are queued.
///
/// The core judges them against its own state: a pane under a real key beside a
/// member row with no session is a pane left over from a consultation that
/// ended, and is hung up. The event that rekeys a pane to its real id is also
/// the one whose `SetMemberSession` writes the row, so a read made while it is
/// still waiting sees that very pair for a Goinkyo that has just been
/// identified (a live run lost two this way, a few milliseconds after each one's
/// discovery). With nothing queued, no event is left that could change either
/// half. The core's `update_goinkyo_awaiting_spawn` relies on this and does not
/// check it.
fn next_event(
    queue: &mut VecDeque<Event>,
    observations_owed: &mut bool,
    observe: impl FnOnce() -> Vec<Event>,
) -> Option<Event> {
    if queue.is_empty() && std::mem::take(observations_owed) {
        queue.extend(observe());
    }
    queue.pop_front()
}

/// The events this tick's Goinkyo looks call for, recorded in the pane log as
/// they are read.
fn goinkyo_events(
    state: &EmporiumState,
    store: &RefCell<Store>,
    app: &App,
    mut pane_log: Option<&mut PaneLog>,
    timings: Option<&mut TickTimings>,
) -> Vec<Event> {
    let mut events = Vec::new();
    for look in gather_goinkyo_looks(state, store, app, timings) {
        if let Some(log) = pane_log.as_deref_mut() {
            log.goinkyo(&look);
        }
        if let Some(observation) = decide_goinkyo(&look) {
            events.push(Event::GoinkyoAwaitingSpawn { observation });
        }
    }
    events
}

/// What each live brigade's Goinkyo row looks like this tick, as a value:
/// [`GoinkyoLook`] is both what [`decide_goinkyo`] decides from and what the
/// pane log writes, so the record and the decision are one reading. A brigade
/// is live when its Director's pane is hosted, on stage or not; a brigade
/// nobody has opened has a Director with no pane, and a Goinkyo row it holds is
/// not started behind the operator's back at startup. Brigades banto has a
/// Goinkyo pane recorded for are read whether or not their Director is hosted,
/// so that an ended consultation is noticed wherever the Director's pane went.
///
/// A store read failure for one brigade says nothing about it: it is not
/// itself evidence the row is gone, so it is read as `roster_read: false`,
/// which decides nothing. Misreporting it as an absent row would release the
/// one-shot guard on a transient glitch rather than an actual ended
/// consultation.
///
/// This does not check whether a pane is already open or in flight for a
/// would-be candidate: that needs the synthetic key `stage_brigade` builds
/// (`SessionKey::new_worker`), which is private to `banto_core::engine`.
/// `update_goinkyo_awaiting_spawn` does that check on the core side instead.
fn gather_goinkyo_looks(
    state: &EmporiumState,
    store: &RefCell<Store>,
    app: &App,
    mut timings: Option<&mut TickTimings>,
) -> Vec<GoinkyoLook> {
    // One look per brigade rather than one per tick: a brigade with a spawn to
    // attempt must not hide the next one's. The ids are the brigades banto has
    // a Goinkyo pane recorded for, and the brigades the store lists. Sorted: a
    // fold that is replayed is handed the same order every run.
    let mut ids: BTreeSet<BrigadeId> = state.goinkyo_brigades().into_iter().collect();
    let listing_start = Instant::now();
    if let Ok(brigades) = store.borrow().list_brigades() {
        ids.extend(brigades.iter().map(|brigade| brigade.id));
    }
    if let Some(t) = timings.as_deref_mut() {
        t.goinkyo_roster += listing_start.elapsed();
    }
    let mut looks = Vec::new();
    for brigade_id in ids {
        let guard = state.goinkyo_guard_facts(brigade_id);
        let roster_start = Instant::now();
        let members = store.borrow().brigade_members_in_join_order(brigade_id);
        if let Some(t) = timings.as_deref_mut() {
            t.goinkyo_roster += roster_start.elapsed();
        }
        let Ok(members) = members else {
            looks.push(GoinkyoLook {
                brigade_id,
                roster_read: false,
                row: GoinkyoRow::Absent,
                director_hosted: false,
                guard,
                cwd: PathBuf::new(),
                roster: Vec::new(),
            });
            continue;
        };
        // The Director whose pane makes the brigade live, and the working
        // directory the rest of the brigade runs in: both from its roster row,
        // as `stage_brigade` reads them, so that nothing depends on which
        // brigade is on stage.
        let director_session = members
            .iter()
            .find(|m| m.role == BrigadeRole::Director)
            .and_then(|m| m.session_id.as_ref());
        let director_hosted = director_session
            .is_some_and(|session| state.screens.contains_key(&SessionKey::from_id(&session.0)));
        let row = match members.iter().find(|m| m.role == BrigadeRole::Goinkyo) {
            None => GoinkyoRow::Absent,
            Some(goinkyo) => match &goinkyo.session_id {
                Some(session) => GoinkyoRow::Session(session.0.clone()),
                None => GoinkyoRow::NoSession,
            },
        };
        // Only the awaiting case reads the cwd and the roster; the decision
        // uses them there and nowhere else, so the rest skip the work.
        let awaiting = matches!(row, GoinkyoRow::NoSession) && director_hosted;
        let (cwd, roster) = if awaiting {
            // Same cwd resolution as `stage_brigade` and `add_worker`: the
            // Director's own row, since a Goinkyo has no cwd of its own, and
            // "." when that row is not resolved.
            let cwd = director_session
                .and_then(|session| app.row_for_id(&session.0))
                .and_then(|row| row.cwd.clone())
                .unwrap_or_else(|| PathBuf::from("."));
            // Already in join order: what the core needs to place the
            // consultation among the Workers (see the field's doc).
            (cwd, members.iter().map(|m| m.token.clone()).collect())
        } else {
            (PathBuf::new(), Vec::new())
        };
        looks.push(GoinkyoLook {
            brigade_id,
            roster_read: true,
            row,
            director_hosted,
            guard,
            cwd,
            roster,
        });
    }
    looks
}

/// The observations the looks call for, one per brigade that has something to
/// say. Only the tests call this; the loop uses the looks, so that it can record
/// them.
#[cfg(test)]
fn gather_goinkyo_observations(
    state: &EmporiumState,
    store: &RefCell<Store>,
    app: &App,
    timings: Option<&mut TickTimings>,
) -> Vec<GoinkyoObservation> {
    gather_goinkyo_looks(state, store, app, timings)
        .iter()
        .filter_map(decide_goinkyo)
        .collect()
}

/// The pane lifecycle log for this UI process, with its start line written
/// first. `None` when banto has no data directory or the file cannot be opened:
/// the UI then runs without a log, as it would without this module.
fn open_pane_log(deps: &Deps) -> Option<PaneLog> {
    let mut log = PaneLog::open(&data_dir_of(deps)?.logs_dir())?;
    log.start(&pane_lines::start_line(
        env!("CARGO_PKG_VERSION"),
        env!("BANTO_BUILD_COMMIT"),
        env!("BANTO_BUILD_DIRTY"),
        SCHEMA_VERSION,
        std::process::id(),
    ));
    Some(log)
}

/// The facts the pane log reads about the pane an Event concerns, taken before
/// the Event is folded. Only the Events that name a pane have any; the rest
/// carry what they need themselves.
fn pane_facts_for(
    state: &EmporiumState,
    app: &App,
    handles: &HashMap<SessionKey, PtyHandle>,
    ev: &Event,
    now: Instant,
) -> pane_lines::PaneFacts {
    let Some(key) = pane_lines::pane_facts_key(ev) else {
        return pane_lines::PaneFacts::default();
    };
    pane_lines::PaneFacts {
        pid: handles.get(key).and_then(PtyHandle::pid),
        ..state.pane_facts(app, key, now)
    }
}

/// The OS error number under a failed spawn, when the failure was an I/O error.
fn os_error_of(err: &anyhow::Error) -> Option<i32> {
    err.downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
}

/// Every brigade with its members, `None` where reading them failed.
type Roster = Vec<(Brigade, Option<Vec<BrigadeMember>>)>;

/// What one tick reads once and every gather of that tick shares: the roster
/// and the live-state files. The relay and the session-move gather each read both
/// a few lines apart, and the live scan is a directory read per tick.
struct TickRead {
    /// Every brigade with its roster, `None` where a read failed; the outer
    /// `None` is a brigade list that could not be read at all.
    brigades: Option<Roster>,
    live: Vec<LiveSession>,
    /// Which of the live files' pids are running as the session that wrote them,
    /// asked of the OS at most once a tick and by whichever gather needs it first
    /// ([`Self::alive_claude`]).
    alive: std::cell::OnceCell<HashSet<u32>>,
}

impl TickRead {
    fn read(
        store: &RefCell<Store>,
        claude_home: &ClaudeHome,
        mut timings: Option<&mut TickTimings>,
    ) -> Self {
        let roster_start = Instant::now();
        let brigades = store.borrow().list_brigades().ok().map(|brigades| {
            brigades
                .into_iter()
                .map(|brigade| {
                    let members = store.borrow().brigade_members(brigade.id).ok();
                    (brigade, members)
                })
                .collect()
        });
        if let Some(t) = timings.as_deref_mut() {
            t.roster += roster_start.elapsed();
        }
        let live_scan_start = Instant::now();
        let live = read_live_sessions(&claude_home.sessions_dir());
        if let Some(t) = timings {
            t.live_scan += live_scan_start.elapsed();
        }
        Self {
            brigades,
            live,
            alive: std::cell::OnceCell::new(),
        }
    }

    /// The live files' pids whose process is running right now and is the
    /// session that wrote the file (`session::alive_claude_pids`). One liveness
    /// pass per tick however many gathers ask: it is the costly part of a tick on
    /// Windows (see `gather_relay_observations_from`), and the relay asks every
    /// tick, so the session-move gather's use of it is free.
    fn alive_claude(&self) -> &HashSet<u32> {
        self.alive
            .get_or_init(|| session::alive_claude_pids(&self.live))
    }
}

/// How many parent-pid hops [`gather_session_moves`] walks looking for a
/// pane's own child pid in a live entry's ancestry — covers `claude`
/// launched through a cmd/npm shim (a couple of hops) with headroom to
/// spare, without letting a pathological chain spin forever.
const MOVE_ANCESTRY_DEPTH: u32 = 5;

/// The id a hosted Claude pane's own process now reports, when it is not the
/// one the pane is keyed by: the same `sessions/<pid>.json` the process has
/// always published starts naming another `sessionId`. `/clear`, `/branch` and
/// an in-pane `/resume` do that; auto-compaction does not (2.1.318 and later
/// compact in place, under the same id).
///
/// Returns the pid of the live entry that said so along with the id.
///
/// `None` while some live entry of a running process still reports `id`: a process cannot report two
/// ids at once, so that is the steady state, and it keeps the (sysinfo-backed)
/// ancestry walks off the every-second path until something changes. Among the
/// entries that descend from `pid` the nearest one wins, so a `claude` the pane's
/// own process started (a `claude -p` from a tool call) is not mistaken for it.
///
/// An entry whose process is gone (`alive`) is a leftover file, not a report, in
/// either direction: a `claude -p` that has exited leaves one behind until its
/// directory is swept, and its pid can sit below the pane; and a dead file still
/// naming the pane's own key would otherwise hide a real `/clear` for as long as
/// it lay there. `held` is the entry an earlier tick named the move on: while it
/// is still alive and still says the same, nothing is walked again.
fn moved_session_id(
    live: &[LiveSession],
    id: &str,
    pid: u32,
    view: &ProcessView,
    held: Option<(u32, &str)>,
) -> Option<(u32, String)> {
    if live
        .iter()
        .any(|entry| entry.session_id.as_deref() == Some(id) && (view.alive)(entry.pid))
    {
        return None;
    }
    if let Some((entry_pid, new_id)) = held
        && (view.alive)(entry_pid)
        && live
            .iter()
            .any(|entry| entry.pid == entry_pid && entry.session_id.as_deref() == Some(new_id))
    {
        return Some((entry_pid, new_id.to_string()));
    }
    live.iter()
        .filter_map(|entry| {
            let new_id = entry.session_id.as_deref().filter(|new| !new.is_empty())?;
            if !(view.alive)(entry.pid) {
                return None;
            }
            let hops = ancestry_distance(entry.pid, pid, view.probe, MOVE_ANCESTRY_DEPTH)?;
            Some((hops, entry.pid, new_id))
        })
        .min_by_key(|(hops, _, _)| *hops)
        .map(|(_, entry_pid, new_id)| (entry_pid, new_id.to_string()))
}

/// What [`moved_session_id`] asks the operating system: the process table (parent
/// links) and which pids are running.
struct ProcessView<'a> {
    probe: &'a dyn ProcessProbe,
    alive: &'a dyn Fn(u32) -> bool,
}

/// What [`gather_session_moves`] asks the outside world beyond the tick's reads.
struct MoveSources<'a> {
    processes: &'a ProcessView<'a>,
    /// Whether Codex's log database says this thread's newest row was written by
    /// this pid, a live process that started no later than the row
    /// (`codex_liveness::is_thread_owned_by`). A closure so the gather is tested
    /// without a database.
    owns_thread: &'a dyn Fn(&str, u32) -> bool,
}

/// A probe that asks the probe it wraps for each pid's parent once and remembers
/// the answer. A pane with no live file of its own (just started, or one nobody
/// reports) never reaches the steady-state check, so it walks every live entry's
/// ancestry, and each hop is a process-table refresh. Panes and entries share
/// most of their chains, so the walk is paid once per pid per gather instead of
/// once per pane and entry. The cache lives for one gather, so a parent that
/// changes is never stale for longer than a tick. Not done: skipping panes
/// younger than a few seconds, which would also delay the first rename of a
/// `/clear` typed right after a start.
struct CachedParents<'a> {
    inner: &'a dyn ProcessProbe,
    parents: RefCell<HashMap<u32, Option<u32>>>,
}

impl ProcessProbe for CachedParents<'_> {
    fn is_alive(&self, pid: u32) -> bool {
        self.inner.is_alive(pid)
    }

    fn parent_pid(&self, pid: u32) -> Option<u32> {
        *self
            .parents
            .borrow_mut()
            .entry(pid)
            .or_insert_with(|| self.inner.parent_pid(pid))
    }

    fn is_alive_matching(&self, pid: u32, proc_start: &str) -> bool {
        self.inner.is_alive_matching(pid, proc_start)
    }
}

/// How many consecutive ticks a pane's process must report the same other id
/// before the move is believed. A live file that goes missing or is read torn for
/// one tick while a nested `claude -p` runs below the pane leaves the nested
/// session as the only entry under it, which one tick alone cannot tell from the
/// pane having moved there; renaming on that and back a second later would rekey
/// the pane, write the member's row and then undo both.
const MOVE_CONFIRM_TICKS: u32 = 2;

/// How many ticks pass before a move that is still observed is sent again. The
/// core refuses a move onto a conversation something else holds (another pane, an
/// open in flight, another member's row) and says so once; what held it can let go
/// later (the open fails, the other pane closes, the row changes), and only the
/// shell can see that the pane's process is still on the new id. Sent every tick,
/// as it was before the watch existed, the refusal also wrote a pane-log line and
/// walked the process tree each second; every fifth tick keeps the retry within a
/// few seconds of the release at a fifth of that. The core's latch keeps the
/// status line to one message however often it is sent.
const MOVE_RESEND_TICKS: u32 = 5;

/// How many consecutive ticks the roster may stay unread before the operator is
/// told that session tracking is paused. One brigade row the store cannot decode
/// (an unrecognised role token) fails the whole roster read for as long as it
/// exists, and with it every pane's tracking, member and solo, without a word.
/// A read that fails for a tick or two (a locked database) is routine and not
/// worth a line.
const MOVE_PAUSED_NOTICE_TICKS: u32 = 10;

/// How many consecutive ticks a Codex member's recorded thread may stay
/// unconfirmed ([`confirm_codex_thread`]) before the operator is told. The two
/// things it waits for, Codex's log row and the reload that lists the new rollout,
/// both lag the hook by a time nobody has measured, and a notice that says "not
/// following" about a thread that is confirmed seconds later reads as a verdict. So
/// it is long, and the wording says "yet".
const UNCONFIRMED_NOTICE_TICKS: u32 = 30;

/// What [`gather_session_moves`] has seen of each pane across ticks: which id it
/// reported, for how many ticks, and when the move was last sent.
///
/// A move is sent when it is believed ([`MOVE_CONFIRM_TICKS`]) and again every
/// [`MOVE_RESEND_TICKS`] while it is still observed, which only a move the core did
/// not apply can be: an applied rename takes the old key away from the pane, so the
/// move is no longer observed under it. The map is rebuilt from each tick's
/// observations, so a pane that went away, or whose process stopped reporting the
/// move, leaves no entry behind; a move that reappears is observed afresh.
#[derive(Default)]
struct SessionMoveWatch {
    seen: HashMap<SessionKey, WatchedMove>,
    /// Codex members whose row records a thread the pane is not confirmed to be
    /// in ([`confirm_codex_thread`]), by pane. Not in `seen`: nothing is sent for
    /// these, and the entry only decides when the operator is told.
    unconfirmed: HashMap<SessionKey, UnconfirmedMove>,
    /// Consecutive ticks the roster was not read in full, and whether this
    /// stretch has been told about already.
    unread_ticks: u32,
    paused_said: bool,
}

/// A Codex thread change not yet tied to its pane, and how long it has been so.
struct UnconfirmedMove {
    new_id: String,
    ticks: u32,
    said: bool,
}

struct WatchedMove {
    new_id: String,
    entry_pid: u32,
    ticks: u32,
    /// The tick count at which the move was last sent; `None` until it is.
    sent_at: Option<u32>,
}

/// A move this tick's live files show, before [`SessionMoveWatch`] decides whether
/// to send it.
struct ObservedMove {
    key: SessionKey,
    entry_pid: u32,
    new_id: String,
    event: Event,
}

impl SessionMoveWatch {
    /// The entry an earlier tick sent this pane's move on, so that its ancestry
    /// is not walked again.
    fn named_entry(&self, key: &SessionKey) -> Option<(u32, &str)> {
        self.seen
            .get(key)
            .filter(|watched| watched.sent_at.is_some())
            .map(|watched| (watched.entry_pid, watched.new_id.as_str()))
    }

    /// This tick's roster read, whole or not: the notice to show once the roster
    /// has stayed unread for [`MOVE_PAUSED_NOTICE_TICKS`] ticks, said once per
    /// stretch (a complete tick ends the stretch, so it can be said again).
    fn roster_notice(&mut self, complete: bool) -> Option<Event> {
        if complete {
            self.unread_ticks = 0;
            self.paused_said = false;
            return None;
        }
        self.unread_ticks = self.unread_ticks.saturating_add(1);
        if self.paused_said || self.unread_ticks < MOVE_PAUSED_NOTICE_TICKS {
            return None;
        }
        self.paused_said = true;
        Some(Event::StartupNotices {
            notices: vec![
                "session tracking paused: the brigade roster could not be read".to_string(),
            ],
        })
    }

    /// This tick's observations in; the events to send out.
    fn admit(&mut self, observed: Vec<ObservedMove>) -> Vec<Event> {
        let mut events = Vec::new();
        let mut next = HashMap::new();
        for ObservedMove {
            key,
            entry_pid,
            new_id,
            event,
        } in observed
        {
            let mut watched = match self.seen.remove(&key) {
                Some(watched) if watched.new_id == new_id => watched,
                _ => WatchedMove {
                    new_id,
                    entry_pid,
                    ticks: 0,
                    sent_at: None,
                },
            };
            watched.entry_pid = entry_pid;
            watched.ticks = watched.ticks.saturating_add(1);
            let due = match watched.sent_at {
                None => watched.ticks >= MOVE_CONFIRM_TICKS,
                Some(at) => watched.ticks - at >= MOVE_RESEND_TICKS,
            };
            if due {
                watched.sent_at = Some(watched.ticks);
                events.push(event);
            }
            next.insert(key, watched);
        }
        self.seen = next;
        events
    }
}

/// Whether the roster of some member other than `own` (the member a move is being
/// named for, if it is one) already records `new_id`: that member's pane may be
/// closed, so no pane holds the id, but `SetMemberSession` moves a session id
/// between rows and would take it from them.
fn row_holds(
    brigades: &[(Brigade, Option<Vec<BrigadeMember>>)],
    new_id: &str,
    own: Option<(BrigadeId, &str)>,
) -> bool {
    brigades.iter().any(|(brigade, members)| {
        members.iter().flatten().any(|member| {
            member.session_id.as_ref().is_some_and(|id| id.0 == new_id)
                && own != Some((brigade.id, member.token.as_str()))
        })
    })
}

/// Detect a hosted pane that has moved onto another session in place:
/// [`Event::MemberSessionForked`] for a brigade member, [`Event::PaneSessionMoved`]
/// for any other pane. Each is sent after the move has held for
/// [`MOVE_CONFIRM_TICKS`] ticks and again every [`MOVE_RESEND_TICKS`] while it is
/// still observed ([`SessionMoveWatch`]); the core's handlers tolerate a repeat.
///
/// A Claude pane is followed through its process's live file
/// ([`moved_session_id`]), member or not. A Codex pane has no such file and its
/// session id is never in Claude's `sessions/<pid>.json`, whose permanent absence
/// would read every tick as "the id moved"; a Codex *member* is followed through
/// what its SessionStart hook recorded instead ([`codex_moved_thread`]), and a Codex
/// pane that is no member is not followed at all, since nothing in it runs the
/// hook. A pane whose product is not known yet (`app` has no row for it) is
/// skipped and caught on a later tick.
///
/// A tick that could not read every brigade's members names no move at all. Which
/// panes are members is read off the roster, and so is whether another member's
/// row already records the id a pane moved onto (`held_by_row`): with one brigade
/// missing, a solo pane might be its member, and a member's move might be onto an
/// id the missing brigade's row holds, which `SetMemberSession` would then take
/// from it without a word. The streaks restart on the next complete tick. A
/// roster that stays unread is told once ([`MOVE_PAUSED_NOTICE_TICKS`]).
fn gather_session_moves(
    read: &TickRead,
    state: &EmporiumState,
    handles: &HashMap<SessionKey, PtyHandle>,
    app: &App,
    watch: &mut SessionMoveWatch,
    sources: &MoveSources,
    timings: Option<&mut TickTimings>,
) -> Vec<Event> {
    let per_pane_start = Instant::now();
    let parents = CachedParents {
        inner: sources.processes.probe,
        parents: RefCell::new(HashMap::new()),
    };
    let view = ProcessView {
        probe: &parents,
        alive: sources.processes.alive,
    };
    let mut observed = Vec::new();
    let roster = read
        .brigades
        .as_ref()
        .filter(|brigades| roster_is_complete(brigades));
    let notice = watch.roster_notice(roster.is_some());
    // Only a complete roster re-examines the Codex members, so only then are the
    // counters taken: a tick that reads nothing must not reset them, or the notice
    // (a latch among them) would be said again after every unreadable tick.
    let mut unconfirmed_before = if roster.is_some() {
        std::mem::take(&mut watch.unconfirmed)
    } else {
        HashMap::new()
    };
    let mut unconfirmed_notices = Vec::new();
    if let Some(roster) = roster {
        let mut member_keys: HashSet<SessionKey> = HashSet::new();
        let agent_of = |id: &str| app.row_for_id(id).map(|row| row.agent);
        let is_claude = |id: &str| agent_of(id) == Some(AgentKind::ClaudeCode);
        for (brigade, members) in roster {
            for member in members.iter().flatten() {
                let Some(session_id) = member.session_id.as_ref() else {
                    continue;
                };
                let old_key = SessionKey::from_id(&session_id.0);
                member_keys.insert(old_key.clone());
                if !state.screens.contains_key(&old_key) {
                    continue;
                }
                if agent_of(&session_id.0) == Some(AgentKind::Codex) {
                    let Some(handle) = handles.get(&old_key) else {
                        continue;
                    };
                    let (Some(pid), Some(new_id)) = (
                        handle.pid(),
                        codex_moved_thread(member, handle.spawned_at()),
                    ) else {
                        continue;
                    };
                    // A move already admitted was tied to the pane then; the log
                    // is not asked again every tick while the core refuses it.
                    let tied = watch
                        .seen
                        .get(&old_key)
                        .is_some_and(|watched| watched.new_id == new_id);
                    match if tied {
                        Ok(())
                    } else {
                        confirm_codex_thread(new_id, pid, app, sources.owns_thread)
                    } {
                        Ok(()) => observed.push(ObservedMove {
                            key: old_key,
                            entry_pid: pid,
                            event: Event::MemberSessionForked {
                                brigade_id: brigade.id,
                                token: member.token.clone(),
                                old_id: session_id.0.clone(),
                                new_id: new_id.to_string(),
                                held_by_row: row_holds(
                                    roster,
                                    new_id,
                                    Some((brigade.id, &member.token)),
                                ),
                            },
                            new_id: new_id.to_string(),
                        }),
                        Err(why) => {
                            let mut entry = match unconfirmed_before.remove(&old_key) {
                                Some(entry) if entry.new_id == new_id => entry,
                                _ => UnconfirmedMove {
                                    new_id: new_id.to_string(),
                                    ticks: 0,
                                    said: false,
                                },
                            };
                            entry.ticks = entry.ticks.saturating_add(1);
                            if !entry.said && entry.ticks >= UNCONFIRMED_NOTICE_TICKS {
                                entry.said = true;
                                unconfirmed_notices.push(format!(
                                    "{} of {}: Codex switched to session {new_id}, \
                                     which banto cannot yet tie to this pane ({why}); \
                                     not following it",
                                    member.token,
                                    brigade_label(None, &brigade.name, brigade.id)
                                ));
                            }
                            watch.unconfirmed.insert(old_key, entry);
                        }
                    }
                    continue;
                }
                if !is_claude(&session_id.0) {
                    continue;
                }
                let Some(pid) = handles.get(&old_key).and_then(PtyHandle::pid) else {
                    continue;
                };
                if let Some((entry_pid, new_id)) = moved_session_id(
                    &read.live,
                    &session_id.0,
                    pid,
                    &view,
                    watch.named_entry(&old_key),
                ) {
                    let held_by_row = row_holds(roster, &new_id, Some((brigade.id, &member.token)));
                    observed.push(ObservedMove {
                        key: old_key,
                        entry_pid,
                        event: Event::MemberSessionForked {
                            brigade_id: brigade.id,
                            token: member.token.clone(),
                            old_id: session_id.0.clone(),
                            new_id: new_id.clone(),
                            held_by_row,
                        },
                        new_id,
                    });
                }
            }
        }
        for (key, handle) in handles {
            if key.is_synthetic() || member_keys.contains(key) || !state.screens.contains_key(key) {
                continue;
            }
            let (true, Some(pid)) = (is_claude(key.as_str()), handle.pid()) else {
                continue;
            };
            if let Some((entry_pid, new_id)) =
                moved_session_id(&read.live, key.as_str(), pid, &view, watch.named_entry(key))
            {
                let held_by_row = row_holds(roster, &new_id, None);
                observed.push(ObservedMove {
                    key: key.clone(),
                    entry_pid,
                    event: Event::PaneSessionMoved {
                        old_id: key.as_str().to_string(),
                        new_id: new_id.clone(),
                        held_by_row,
                    },
                    new_id,
                });
            }
        }
    }
    let mut events = watch.admit(observed);
    events.extend(notice);
    if !unconfirmed_notices.is_empty() {
        events.push(Event::StartupNotices {
            notices: unconfirmed_notices,
        });
    }
    if let Some(t) = timings {
        t.session_moves += per_pane_start.elapsed();
    }
    events
}

/// The thread a hosted Codex member's row says its pane has moved onto: the
/// session id `banto _hook` last ran under, when it differs from the one the row
/// (and the pane's key) holds and the hook ran in this very process
/// (`spawned_at`; the column is never cleared, so an older value is a previous
/// process's). Codex switches thread inside one process with `/new`, `/clear`,
/// `/fork` and an in-pane `/resume`, and the hook fires at the next typed turn with
/// the new thread's id (measured on Codex 0.169.0). Which command switched is
/// never asked: only what the hook recorded is read.
fn codex_moved_thread(member: &BrigadeMember, spawned_at: SystemTime) -> Option<&str> {
    let held = member.session_id.as_ref()?;
    let briefed = member.briefed_session_id.as_ref()?;
    (briefed != held && member.briefed_at.is_some_and(|at| at >= spawned_at))
        .then_some(briefed.0.as_str())
}

/// Whether `new_id` is a thread the pane whose child is `pid` is in; `Err` is the
/// reason it is not confirmed, for the operator. Both must hold. Codex's log
/// database says the thread's newest row was written by that process
/// (`owns_thread`, which carries the pid-recycling check): the hook's own word is
/// not taken as proof. And the thread is a listed Codex session.
///
/// "Listed" is the only defence against a sub-agent thread of the same process,
/// whose log rows name the pane's pid too. No sub-agent thread has been seen to run
/// the hook (it may not), so this is a guard, not an observed case. It rests on the
/// provider recognising the sub-agent `source` shape (`provider::codex`): a Codex
/// release that changed that shape would list such a thread, and the pane would be
/// re-pointed onto a sub-agent's conversation: the wrong identity, though never a
/// double open, since the held-key refusal still applies.
///
/// The log write and the reload that lists the new rollout can each land after
/// the hook, so the caller waits before it says anything.
fn confirm_codex_thread(
    new_id: &str,
    pid: u32,
    app: &App,
    owns_thread: &dyn Fn(&str, u32) -> bool,
) -> Result<(), &'static str> {
    if app.row_for_id(new_id).map(|row| row.agent) != Some(AgentKind::Codex) {
        return Err("it is not a listed Codex session");
    }
    if !owns_thread(new_id, pid) {
        return Err("Codex's log does not show this pane's process writing it");
    }
    Ok(())
}

/// Whether every brigade's members were read: only then is a pane that is in none
/// of the rosters known to be in no brigade, and only then is "no member's row holds
/// this id" known.
fn roster_is_complete(brigades: &Roster) -> bool {
    brigades.iter().all(|(_, members)| members.is_some())
}

/// One tick of the move gather against `watch`, every process alive but those
/// `dead` names, the real process table asked for parents.
#[cfg(test)]
fn gather_session_moves_tick(
    watch: &mut SessionMoveWatch,
    dead: &[u32],
    state: &EmporiumState,
    store: &RefCell<Store>,
    claude_home: &ClaudeHome,
    handles: &HashMap<SessionKey, PtyHandle>,
    app: &App,
) -> Vec<Event> {
    gather_session_moves_viewing(
        watch,
        &ProcessView {
            probe: &SysinfoProbe,
            alive: &|pid| !dead.contains(&pid),
        },
        state,
        store,
        claude_home,
        handles,
        app,
    )
}

/// One tick of the move gather with the process table `view` names.
#[cfg(test)]
fn gather_session_moves_viewing(
    watch: &mut SessionMoveWatch,
    view: &ProcessView,
    state: &EmporiumState,
    store: &RefCell<Store>,
    claude_home: &ClaudeHome,
    handles: &HashMap<SessionKey, PtyHandle>,
    app: &App,
) -> Vec<Event> {
    let read = TickRead::read(store, claude_home, None);
    let sources = MoveSources {
        processes: view,
        owns_thread: &|_, _| false,
    };
    gather_session_moves(&read, state, handles, app, watch, &sources, None)
}

/// What the loop sends for a move that has held: the second tick's events, with
/// every process alive. The first tick alone sends nothing
/// (`a_move_is_believed_only_when_it_holds_for_two_ticks`).
#[cfg(test)]
fn gather_session_moves_reading(
    state: &EmporiumState,
    store: &RefCell<Store>,
    claude_home: &ClaudeHome,
    handles: &HashMap<SessionKey, PtyHandle>,
    app: &App,
    timings: Option<&mut TickTimings>,
) -> Vec<Event> {
    let mut watch = SessionMoveWatch::default();
    let read = TickRead::read(store, claude_home, None);
    let view = ProcessView {
        probe: &SysinfoProbe,
        alive: &|_| true,
    };
    let sources = MoveSources {
        processes: &view,
        owns_thread: &|_, _| false,
    };
    gather_session_moves(&read, state, handles, app, &mut watch, &sources, None);
    gather_session_moves(&read, state, handles, app, &mut watch, &sources, timings)
}

#[cfg(test)]
fn gather_relay_observations(
    state: &EmporiumState,
    store: &RefCell<Store>,
    claude_home: &ClaudeHome,
    codex_home: Option<&CodexHome>,
    app: &App,
    mut timings: Option<&mut TickTimings>,
    glance_gate: &mut GlanceGate,
) -> Vec<RelayObservation> {
    let read = TickRead::read(store, claude_home, timings.as_deref_mut());
    gather_relay_observations_from(&read, state, store, codex_home, app, timings, glance_gate)
}

/// This member's role briefing, or `None` when the configured template for
/// its role is empty. Only the Claude path uses the returned string as launch
/// argv; a Codex member's briefing is rendered from the same template in the
/// `banto _hook` process instead (see [`crate::briefing`]).
fn member_briefing(
    deps: &Deps,
    brigade_id: BrigadeId,
    token: &str,
    role: BrigadeRole,
) -> Option<String> {
    let peers = crate::briefing::peers_of(&deps.store.borrow(), brigade_id, role);
    let request = briefing_request(deps, brigade_id, role);
    // Always the Claude form: a Codex member discards this string
    // (`build_open_launch`) and gets its briefing from the hook.
    crate::briefing::compose(
        deps.brigade,
        AgentKind::ClaudeCode,
        role,
        brigade_id,
        token,
        &peers,
        request.as_deref(),
    )
}

/// What [`member_briefing`] would be compared against: the same briefing with
/// `{peers}` left a hole. Everything but the roster comes from the same inputs
/// as [`member_briefing`] — the one [`briefing_request`] and the same
/// `crate::briefing::compose` underneath — so the two cannot drift apart.
fn member_briefing_pattern(
    deps: &Deps,
    brigade_id: BrigadeId,
    token: &str,
    role: BrigadeRole,
) -> Option<BriefingPattern> {
    let request = briefing_request(deps, brigade_id, role);
    crate::briefing::compose_pattern(deps.brigade, role, brigade_id, token, request.as_deref())
}

/// The consultation request path a Goinkyo's briefing names as `{request}`;
/// `None` for the other roles, whose templates have none.
fn briefing_request(deps: &Deps, brigade_id: BrigadeId, role: BrigadeRole) -> Option<String> {
    (role == BrigadeRole::Goinkyo)
        .then(|| data_dir_of(deps).and_then(|dir| goinkyo_request_path(&dir, brigade_id)))
        .flatten()
}

/// Whether a launch needs `--system-prompt-snapshot off` to get its briefing to
/// the model. Claude records the system prompt on a conversation's first request
/// and reuses it on every request after, `--resume` included, so a resumed
/// member's `--append-system-prompt` text is ignored unless the recorded copy
/// already is that text — or the conversation has been compacted since, which
/// makes the next default launch record afresh (`banto_io::claude_briefing` has
/// the rule and the measurements). A creation records whatever it is given, so it
/// never needs the flag; neither does a launch with no briefing, or Codex, whose
/// briefing does not travel this way.
///
/// The transcript read lives here, at the edge, and is read-only. Every
/// transcript that holds the session id is read, and one that cannot be found or
/// read asks for the flag: not knowing is never taken for "nothing to deliver",
/// and which copy Claude resumes from is unmeasured, so one needing the flag is
/// enough.
fn resume_needs_snapshot_off(
    deps: &Deps,
    target: &SessionToOpen,
    brigade: Option<&(BrigadeId, MemberToken, BrigadeRole)>,
    briefing: Option<&str>,
) -> bool {
    if target.agent != AgentKind::ClaudeCode || target.id.is_empty() || briefing.is_none() {
        return false;
    }
    let Some((brigade_id, token, role)) = brigade else {
        return false;
    };
    let Some(pattern) = member_briefing_pattern(deps, *brigade_id, token, *role) else {
        return true;
    };
    claude_briefing::resume_needs_snapshot_off(
        &deps.claude_home.projects_dir(),
        &target.id,
        &pattern,
    )
}

/// The data directory this emporium works in: the one it resolved at startup
/// (`EmporiumSettings::child`), which is also what it hands every banto process it
/// starts. `Deps::child` is `None` only in a test, which then gets the platform's.
fn data_dir_of(deps: &Deps) -> Option<DataDir> {
    match deps.child {
        Some(child) => child.data_dir(),
        None => DataDir::default_root().map(DataDir::new),
    }
}

/// Where `crate::mcp::tool_consult_goinkyo` wrote this brigade's consultation
/// request: the same `DataDir` path the `_mcp` process was handed, so the two
/// cannot disagree about where the file is. `None` for a brigade id banto does
/// not issue.
fn goinkyo_request_path(data_dir: &DataDir, brigade_id: BrigadeId) -> Option<String> {
    data_dir
        .goinkyo_request_path(brigade_id)
        .map(|path| path.to_string_lossy().into_owned())
}

/// The `BANTO_*` variables a brigade member's child is launched with.
///
/// Member identity travels in the environment rather than on the argv because
/// a Codex member's `SessionStart` hook is trusted by a hash of its *command
/// string*: putting the token in that command would make every member a
/// different hook needing its own approval, and worse, they would fight over
/// one trust slot. An untrusted hook is then dropped in silence — no error,
/// no briefing (docs/notes/codex-briefing-spike.md). The environment is
/// outside that hash, so one command serves the whole cell.
///
/// Set for Claude members too, though nothing reads it there yet: one launch
/// path is easier to reason about than two, and a member knowing its own
/// token is useful regardless of product.
pub(crate) fn brigade_env(
    brigade: Option<&(BrigadeId, MemberToken, BrigadeRole)>,
    child: Option<&crate::startup::Resolved>,
) -> Vec<(String, String)> {
    let Some((brigade_id, token, role)) = brigade else {
        return Vec::new();
    };
    let mut env = vec![
        ("BANTO_BRIGADE".to_string(), brigade_id.to_string()),
        ("BANTO_MEMBER".to_string(), token.clone()),
        ("BANTO_ROLE".to_string(), role.as_token().to_string()),
    ];
    // The same channel carries where this banto found its config and database:
    // the hook's command line can hold neither a path (it is hashed, and cannot
    // be quoted) nor a flag, so it reads them from here.
    env.extend(child.map(|child| child.child_env()).unwrap_or_default());
    env
}

/// Everything added to the environment of a pane's child, decided here by agent
/// kind so that no launch path can forget it: the brigade variables
/// ([`brigade_env`]) plus, for Claude Code, `CLAUDE_CODE_NATIVE_CURSOR=1`.
///
/// Without it Claude draws its input caret by a server-side flag
/// (`tengu_native_cursor`) that it caches in the Claude home's `.claude.json`.
/// A home with no cache has the flag off, and Claude then hides the terminal
/// cursor: `banto_core::input_line` read an empty prompt as `HasText` and held
/// every nudge to that pane for good. The operator's home caches the flag on,
/// which can change any day. Claude Code 2.1.327 checks this variable before the
/// flag, so setting it removes the dependence on the home.
fn pane_env(
    agent: AgentKind,
    brigade: Option<&(BrigadeId, MemberToken, BrigadeRole)>,
    child: Option<&crate::startup::Resolved>,
) -> Vec<(String, String)> {
    let mut env = brigade_env(brigade, child);
    if agent == AgentKind::ClaudeCode {
        env.push(("CLAUDE_CODE_NATIVE_CURSOR".to_string(), "1".to_string()));
    }
    env
}

/// Write a per-member `--mcp-config` file wiring the embedded claude to
/// banto's own MCP server (`banto _mcp`) with this member's brigade
/// identity, and return its path. Named by `(brigade_id, token)` rather than
/// the session id, since that's the only identity known upfront for a
/// freshly-spawned Worker. Lives under banto's own data dir, never under
/// `~/.claude`.
fn write_mcp_config(
    data_dir: &DataDir,
    exe: &Path,
    brigade_id: BrigadeId,
    token: &str,
    role: BrigadeRole,
    session_id: Option<&str>,
    child_args: &[String],
) -> Result<PathBuf> {
    let config = mcp_config_json(exe, child_args, brigade_id, token, role, session_id);
    // A token banto did not issue (a hand-edited store) gets no file rather than
    // one the sweep could not tell from a stranger's: the spawn fails instead.
    let path = data_dir
        .member_config_path(brigade_id, token)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no configuration file name for member {token:?} of brigade {brigade_id}"
            )
        })?;
    std::fs::create_dir_all(data_dir.mcp_dir())?;
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
    Ok(path)
}

/// The `--mcp-config` file's content: banto's own executable as the `banto`
/// MCP server, with this member's brigade identity on its argv.
pub(crate) fn mcp_config_json(
    exe: &Path,
    child_args: &[String],
    brigade_id: BrigadeId,
    token: &str,
    role: BrigadeRole,
    session_id: Option<&str>,
) -> serde_json::Value {
    let mut args = vec!["_mcp".to_string()];
    args.extend(child_args.iter().cloned());
    args.extend([
        "--brigade".to_string(),
        brigade_id.to_string(),
        "--member".to_string(),
        token.to_string(),
        "--role".to_string(),
        role.as_token().to_string(),
    ]);
    if let Some(session_id) = session_id {
        args.push("--session".to_string());
        args.push(session_id.to_string());
    }
    serde_json::json!({
        "mcpServers": {
            "banto": {
                "command": exe.to_string_lossy(),
                "args": args,
            }
        }
    })
}

/// How long a pane's synchronized-update block (DECSET 2026,
/// [`Screen::in_synchronized_update`]) is honored without its closing
/// `?2026l` before this draws the pane live anyway — the bound that keeps a
/// child that hangs or dies mid-frame from freezing its pane forever.
/// `banto-core` has no clock (`docs/DISCIPLINE.md` §2/§3), so this deadline
/// lives on the shell side, not on `Screen` itself.
///
/// Matches kitty's own default for this exact wire form: `screen_pause_rendering`
/// (`kitty/screen.c`) is reached from the DECSET/DECRST 2026 handler with no
/// caller-supplied duration, and falls back to `for_in_ms = 2000`. mintty
/// implements the feature's older DCS form (`ESC P = 1 s`, `src/termout.c`)
/// with a much shorter 150ms default (420ms hard cap even on request) — but
/// that number is tuned for a wire form nothing banto hosts sends; kitty's
/// is the same DECSET 2026 path both Codex and Claude Code actually use, so
/// it is the closer precedent.
const SYNC_UPDATE_TIMEOUT: Duration = Duration::from_millis(2000);

/// The last known-good painted cells for one pane, so a synchronized-update
/// block (DECSET 2026) can be honored: what the operator last actually saw,
/// kept ready to blit back in place of a live repaint. Caching the painted
/// `Buffer` `paint_screen` produces, rather than a `Text`/`Paragraph` recipe
/// that would need re-rendering, means this outlives whichever widget paints
/// the live grid.
struct PaneRenderCache {
    buffer: Buffer,
    /// The child's cursor as of `buffer`'s capture (absolute frame
    /// coordinates), or `None` if it was hidden or off-pane — frozen
    /// alongside the cells so a held update does not draw *today's* cursor
    /// over *yesterday's* content, the same tearing this whole cache exists
    /// to prevent.
    cursor: Option<(u16, u16)>,
    /// When this pane's *current* synchronized-update block was first
    /// observed open, for [`SYNC_UPDATE_TIMEOUT`]. `None` outside of one —
    /// including the tick the deadline elapses on, since expiring is itself
    /// what ends the hold, the same as an ordinary `?2026l` would.
    ///
    /// The spec (gitlab.com/gnachman/iterm2 wiki, "Synchronized Updates")
    /// says a Begin-sync received while already inside one should extend
    /// the timeout. This does not: it measures from the first observed
    /// open and never revisits that once set. [`Screen::in_synchronized_update`]
    /// is a bool, not an edge — a repeated `?2026h` while already open is
    /// genuinely invisible from here, indistinguishable from one long
    /// block, so there is no re-open event to extend *from*. Reconstructing
    /// one would mean diffing consecutive polls to infer a transition that
    /// happened between them, for a case neither child this hosts is known
    /// to produce; judged not worth the complexity.
    opened_at: Option<Instant>,
}

/// Paint one pane's content area into `content` of the live screen straight
/// from `vt100`, with no caching involved — the one place that actually
/// builds pane pixels, so [`paint_pane`]'s live and cache-refresh paths stay
/// identical by construction. The cursor position returned alongside is
/// already absolute (frame coordinates, not pane-relative) and already
/// `None` for hidden or out-of-bounds — [`draw`]'s own `focused_tile`/
/// scrollback gate is the only thing left for the caller to apply.
fn paint_live(screen: &Screen, content: Rect) -> (Buffer, Option<(u16, u16)>) {
    let mut buffer = Buffer::empty(content);
    paint::paint_screen(screen.screen(), content, &mut buffer);
    let cursor = if screen.screen().hide_cursor() {
        None
    } else {
        let (row, col) = screen.screen().cursor_position();
        let (x, y) = (content.x + col, content.y + row);
        (x < content.x + content.width && y < content.y + content.height).then_some((x, y))
    };
    (buffer, cursor)
}

/// Paint one pane into `frame_buffer` at `content`, returning the cursor
/// position to draw for it (if any) — absolute frame coordinates, for the
/// caller to gate on focus/scrollback the same as before this cache
/// existed. While `screen` is inside a synchronized-update block it has not
/// yet closed — the cached buffer still matches `content`'s size, and that
/// block is still within [`SYNC_UPDATE_TIMEOUT`] of opening — this blits the
/// last complete frame and cursor from `cache` instead of a grid the child
/// explicitly asked not to be shown mid-draw. Otherwise it paints live and
/// refreshes `cache` for next time — including when the cached size no
/// longer matches `content` (a resize mid-hold invalidates it; there is
/// nothing meaningful to blit at the wrong size).
///
/// A pane's very first paint seeds `cache` with an empty, correctly-sized
/// buffer and no cursor, rather than a live one: painting live here and
/// honoring it below would be self-defeating for the one case that actually
/// matters — a child that is *already* mid-update by the time its pane is
/// first drawn — since there is no earlier known-good frame to fall back on
/// anyway. Blank for up to `SYNC_UPDATE_TIMEOUT` beats leaking a frame the
/// child asked not to be shown.
fn paint_pane(
    frame_buffer: &mut Buffer,
    cache: &mut HashMap<SessionKey, PaneRenderCache>,
    key: &SessionKey,
    screen: &Screen,
    content: Rect,
    tick: Instant,
) -> Option<(u16, u16)> {
    let entry = cache.entry(key.clone()).or_insert_with(|| PaneRenderCache {
        buffer: Buffer::empty(content),
        cursor: None,
        opened_at: None,
    });

    if !screen.in_synchronized_update() {
        entry.opened_at = None;
    } else if entry.buffer.area == content {
        let opened_at = *entry.opened_at.get_or_insert(tick);
        if tick.duration_since(opened_at) < SYNC_UPDATE_TIMEOUT {
            frame_buffer.merge(&entry.buffer);
            return entry.cursor;
        }
        // Held past the deadline with no closing `?2026l`: stop honoring it
        // and fall through to painting live, same as a close would.
    }

    let (live, cursor) = paint_live(screen, content);
    frame_buffer.merge(&live);
    entry.buffer = live;
    entry.cursor = cursor;
    cursor
}

fn draw(
    frame: &mut ratatui::Frame,
    app: &App,
    state: &EmporiumState,
    now: SystemTime,
    tick: Instant,
    pane_render_cache: &mut HashMap<SessionKey, PaneRenderCache>,
) -> Option<Position> {
    let mut frame_cursor = None;
    let full_area = frame.area();
    let focus = state.focus;
    // The same geometry the core sizes the PTYs and hit-tests the mouse with.
    let geometry = state.geometry(full_area);
    let areas = geometry.areas;
    // What a mouse gesture in progress adds on top of the finished frame.
    let overlay = state.drag_overlay(full_area);
    let marked_tile = overlay.as_ref().and_then(|o| o.marked.as_ref());

    let sidebar_title = if app.mode() == Mode::Search {
        format!("/ {}", app.query())
    } else {
        format!("banto · waiting {}", state.visible_waiting_count())
    };
    let sidebar_block = Block::bordered()
        .title(sidebar_title)
        .border_style(border_style(focus == Focus::Sidebar, false));
    let sidebar_inner = sidebar_block.inner(areas.sidebar);
    frame.render_widget(sidebar_block, areas.sidebar);
    view::render_list(frame, app, sidebar_inner, now);

    view::render_summary(frame, app, areas.summary, now);

    let tiles = &geometry.tiles;
    if tiles.is_empty() {
        let block = Block::bordered()
            .title(Span::styled(
                title_text("session"),
                Style::default().fg(Color::Reset),
            ))
            .border_style(border_style(false, false));
        let content = block.inner(areas.pane);
        frame.render_widget(block, areas.pane);
        frame.render_widget(
            Paragraph::new(
                "Select a session and press Enter.\n\
                 B brigade/disband · q/Esc quit.",
            ),
            content,
        );
    } else {
        let focused_key = state.stage.focused_key().cloned();
        for (index, (key, rect)) in tiles.iter().enumerate() {
            let Some(screen) = state.screens.get(key) else {
                continue;
            };
            // A tile with no rows or no columns is not on screen: it draws no
            // border, label, status, buttons, strip or pane, and writes nothing
            // onto the tile placed at the same row.
            if banto_core::layout::is_empty(*rect) {
                continue;
            }
            let collapsed = geometry.collapsed[index];
            // A collapsed tile is never the focused one (the core keeps focus
            // off it), so its border carries attention only.
            let focused_tile =
                !collapsed && focus == Focus::Pane && focused_key.as_ref() == Some(key);
            let waiting = state.attention_panes().contains(key);
            let marked = marked_tile == Some(key);
            let mut status = tile_status(state, app, key);
            // The one place a tile's title row is drawn, collapsed or open.
            let content = paint_tile_title(
                frame.buffer_mut(),
                &geometry,
                &TitleRow {
                    key,
                    rect: *rect,
                    label: &state.tile_label(key),
                    status: &status,
                    focused: focused_tile,
                    waiting,
                    marked,
                    soloed: state.soloed_key() == Some(key),
                },
            );
            if collapsed {
                // The child keeps running untouched; only its facts are shown.
                let facts = state.member_facts(key);
                status.inbound = facts
                    .and_then(|facts| facts.inbound.as_ref())
                    .map(strip_message);
                status.sent = facts
                    .and_then(|facts| facts.sent.as_ref())
                    .map(strip_message);
                render_strip(frame.buffer_mut(), content, &status, now);
                continue;
            }
            let cursor = paint_pane(
                frame.buffer_mut(),
                pane_render_cache,
                key,
                screen,
                content,
                tick,
            );
            // The position `paint_pane` hands back is always what was
            // *painted* this frame — the live cursor, or the frozen one
            // alongside a held update's cells — never adjusted for
            // scrollback (`Screen::scroll`'s own doc): drawn while scrolled
            // back, it would sit on top of whatever historical text happens
            // to be at that row, implying "you can type here" over content
            // that isn't live at all.
            if focused_tile
                && screen.scrollback() == 0
                && let Some((x, y)) = cursor
            {
                let position = Position::new(x, y);
                frame.set_cursor_position(position);
                frame_cursor = Some(position);
            }
        }
    }
    // Panes that no longer exist (session closed/dismissed) would otherwise
    // linger in the cache for the life of the process — nothing ever removes
    // a `SessionKey` entry on its own.
    pane_render_cache.retain(|key, _| state.screens.contains_key(key));

    // A gesture in progress is previewed over the finished frame; the
    // geometry underneath does not move until the release. The moved tile is
    // already marked (`tile_block`); then the line, then the ghost on top.
    // All of it goes into the composed buffer, so the frame-skip sees it
    // appear and go.
    if let Some(overlay) = &overlay {
        if let Some((line, axis)) = overlay.line {
            paint_drag_line(frame.buffer_mut(), line, axis);
        }
        if let Some(ghost) = &overlay.ghost {
            paint_ghost(frame.buffer_mut(), ghost);
        }
    }

    render_status_bar(frame, app, state, areas.status);

    if let Some(modal) = app.modal() {
        // Read at the moment of drawing, from the panes hosted now: the close
        // dialog stays open while panes end and start, and Enter acts on what
        // is hosted when it is pressed.
        let close = match modal {
            banto_core::app::Modal::ConfirmCloseBrigade { brigade_id, .. } => {
                Some(state.close_reading(app, *brigade_id, Instant::now()))
            }
            _ => None,
        };
        banto_tui::render_modal::render_modal(frame, modal, full_area, close.as_ref());
    }

    frame_cursor
}

/// The colour of a mouse gesture: the line previewing a boundary drag, a tile
/// being moved and the ghost of its label. Neither the focus cyan nor the
/// waiting magenta (`border_style`), so it cannot be mistaken for either.
const DRAG_LINE_COLOR: Color = Color::Yellow;

/// A stored message glance as the strip draws it.
fn strip_message(glance: &MessageGlance) -> StripMessage {
    let millis = u64::try_from(glance.created_at_ms).unwrap_or(0);
    StripMessage {
        at: SystemTime::UNIX_EPOCH + Duration::from_millis(millis),
        text: glance.line.clone(),
    }
}

/// The colour of a tile's border: the gesture colour while it is being moved,
/// else focus, waiting and plain as `border_style` has them.
fn tile_border_color(focused: bool, waiting: bool, marked: bool) -> Color {
    if marked {
        DRAG_LINE_COLOR
    } else {
        border_style(focused, waiting).fg.unwrap_or(Color::Reset)
    }
}

/// Paint `key`'s title-row buttons over its border row, in the border's own
/// colour. The soloed tile's `[S]` is reversed while its solo is active.
fn paint_tile_buttons(
    buffer: &mut Buffer,
    geometry: &banto_core::layout::Geometry,
    key: &SessionKey,
    color: Color,
    soloed: bool,
) {
    for button in geometry
        .buttons()
        .iter()
        .filter(|button| &button.key == key)
    {
        let mut style = Style::default().fg(color);
        if soloed && button.kind == ButtonKind::Solo {
            style = style.add_modifier(Modifier::REVERSED);
        }
        buffer.set_stringn(
            button.rect.x,
            button.rect.y,
            button.text,
            usize::from(button.rect.width),
            style,
        );
    }
}

/// What a staged tile says about its pane's state, as its title row draws it
/// (and a collapsed tile's body adds the messages): the one place the product and
/// the activity are decided, so no two tiles or states can disagree. The
/// activity is `EmporiumState::pane_word`: waiting when the pane
/// waits on the operator (the decision the marks, the count and the bell read,
/// and the member's live status of this tick), else busy or idle from what banto
/// has read of it (the decision the quit question uses), else none, with a mark
/// beside an idle one whose turn has ended but whose background command still
/// runs (`EmporiumState::pane_background`); the product is the row's, else the
/// one the relay resolved before there is a row. Computed
/// per staged tile on every frame, by a few map lookups.
fn tile_status(state: &EmporiumState, app: &App, key: &SessionKey) -> StripData {
    let row = app.row_for_id(key.as_str());
    let facts = state.member_facts(key);
    StripData {
        agent: row
            .map(|row| row.agent)
            .or_else(|| facts.and_then(|facts| facts.agent)),
        activity: state.pane_word(app, key),
        inbound: None,
        sent: None,
        // Only a Director's tile has one, and it stays while that pane is focused:
        // it is "not answered yet", not "not looked at yet".
        request: state.has_request(key),
        background: state.pane_background(app, key),
    }
}

/// Everything that decides how one tile's title row looks.
struct TitleRow<'a> {
    key: &'a SessionKey,
    rect: Rect,
    label: &'a str,
    status: &'a StripData,
    focused: bool,
    waiting: bool,
    marked: bool,
    soloed: bool,
}

/// Draw a tile's title row — the border, the label, the status and the buttons —
/// for every staged tile, collapsed or open, and return the tile's content area
/// (inside the border). The only place that does it, so the two states cannot
/// look different: the label (a tab in every state: reversed cyan when focused,
/// which a collapsed tile never is, otherwise `pane_title`'s slate), then
/// `Agent · word` as far as there is room, right-anchored against the buttons
/// and with no background, then `[S]` and `[_]` or `[+]`. Also while the tile
/// is being dragged: the gesture colours the border and the label, and the
/// status keeps its own colours on top of that; the drop line and the ghost
/// label are painted over the finished frame, so where they cross it they win.
fn paint_tile_title(
    buffer: &mut Buffer,
    geometry: &banto_core::layout::Geometry,
    title: &TitleRow,
) -> Rect {
    let block = tile_block(title.label, title.focused, title.waiting, title.marked);
    let content = block.inner(title.rect);
    ratatui::widgets::Widget::render(block, title.rect, buffer);
    paint_tile_buttons(
        buffer,
        geometry,
        title.key,
        tile_border_color(title.focused, title.waiting, title.marked),
        title.soloed,
    );
    paint_title_status(
        buffer,
        geometry,
        title.key,
        title.rect,
        title.label,
        title.status,
    );
    content
}

/// Draw `status` on `key`'s title row between its label and its buttons. The
/// label's cells and the buttons' cells are the geometry's own and are not
/// touched; this only uses what is left of the border between them, so neither
/// the label nor a button moves and no hit-test changes.
fn paint_title_status(
    buffer: &mut Buffer,
    geometry: &banto_core::layout::Geometry,
    key: &SessionKey,
    tile: Rect,
    label: &str,
    status: &StripData,
) {
    let (_, label_end) = banto_core::layout::title_columns(tile, label);
    // One border cell stays between the label and the status, and one between
    // the status and the first button (or the corner).
    let from = label_end.saturating_add(1);
    let right_edge = geometry
        .buttons()
        .iter()
        .filter(|button| &button.key == key)
        .map(|button| button.rect.x)
        .min()
        .unwrap_or(tile.right().saturating_sub(1));
    let to = right_edge.saturating_sub(1);
    if to > from {
        render_title_status(buffer, tile.y, from, to, status);
    }
}

/// A tile's frame. A tile being moved is drawn in the gesture colour — border
/// and title, replacing whichever focus, waiting or plain style it would have
/// had — from the press to the end of the gesture. The title text is the
/// same either way: its cells are the move handle and the resize handle's
/// exclusion (`layout::title_columns`).
fn tile_block(label: &str, focused: bool, waiting: bool, marked: bool) -> Block<'static> {
    if marked {
        return Block::bordered()
            .title(Span::styled(
                title_text(label),
                Style::default()
                    .fg(DRAG_LINE_COLOR)
                    .add_modifier(Modifier::REVERSED),
            ))
            .border_style(Style::default().fg(DRAG_LINE_COLOR));
    }
    Block::bordered()
        .title(pane_title(label, focused))
        .border_style(border_style(focused, waiting))
}

/// Paint the moved tile's label following the pointer, in reversed gesture
/// colour, over whatever the frame had there.
fn paint_ghost(buffer: &mut Buffer, ghost: &Ghost) {
    let style = Style::default()
        .fg(DRAG_LINE_COLOR)
        .add_modifier(Modifier::REVERSED);
    let room = buffer.area.right().saturating_sub(ghost.x);
    if !buffer.area.contains(Position::new(ghost.x, ghost.y)) {
        return;
    }
    let width = unicode_width::UnicodeWidthStr::width(ghost.text.as_str()).min(usize::from(room));
    for x in ghost.x..ghost.x + width as u16 {
        buffer[(x, ghost.y)].reset();
    }
    buffer.set_stringn(ghost.x, ghost.y, &ghost.text, usize::from(room), style);
}

/// Paint the drag preview: a heavy line over `line`'s cells, replacing
/// whatever the frame had there (a border, or a child's text).
fn paint_drag_line(buffer: &mut Buffer, line: Rect, axis: Axis) {
    let symbol = match axis {
        Axis::Vertical => "\u{2503}",
        Axis::Horizontal => "\u{2501}",
    };
    let area = line.intersection(buffer.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buffer[(x, y)];
            cell.reset();
            cell.set_symbol(symbol);
            cell.set_style(Style::default().fg(DRAG_LINE_COLOR));
        }
    }
}

/// Bottom status bar: emporium key hints (or a transient status, or — while
/// a prefix chord is armed — the pending-prefix hint) on the left, the match
/// count on the right. Once quitting has been decided the closing message
/// replaces all of those, whatever else the line had to say.
fn render_status_bar(frame: &mut ratatui::Frame, app: &App, state: &EmporiumState, area: Rect) {
    let counts = format!("[{}/{}]", app.filtered_len(), app.total_len());
    // The badge shares the right-hand segment with the counts, never the left
    // one the status text uses, so no message can displace it. On a terminal too
    // narrow for both, the left text has already shrunk to nothing and the
    // segment is clipped from its right end: the counts go first, then the
    // badge's tail.
    let badge = state
        .bell_when_focused()
        .then_some(banto_tui::hints::BELL_BADGE);
    let badge_width = badge.map_or(0, |text| text.chars().count() as u16 + 1);
    let counts_width = counts.chars().count() as u16;
    let [left, right] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(badge_width + counts_width),
    ])
    .areas(area);

    let (text, color) = if app.should_quit() {
        (state.closing_text(), Color::Yellow)
    } else if state.prefix_armed.is_some() {
        (banto_tui::hints::PREFIX_HINTS.to_string(), Color::Cyan)
    } else {
        match state.displayed_status() {
            Some(message) => (message.to_string(), Color::Yellow),
            None => (
                banto_tui::hints::idle_hints(&banto_tui::hints::IdleHintsInput {
                    focus: state.focus,
                    mode: app.mode(),
                    archived_visible: app.any_archived_visible(),
                    prefix_label: &state.prefix_label(),
                    focusable_tiles: state.focusable_tile_count(),
                }),
                Color::Gray,
            ),
        }
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(color))),
        left,
    );
    let mut right_spans = Vec::new();
    if let Some(badge) = badge {
        right_spans.push(Span::styled(
            badge,
            Style::default()
                .fg(WAITING_ACTIVITY_COLOR)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ));
        right_spans.push(Span::raw(" "));
    }
    right_spans.push(Span::styled(
        counts,
        Style::default().add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(Line::from(right_spans)), right);
}

fn border_style(focused: bool, waiting: bool) -> Style {
    Style::default().fg(if focused {
        Color::Cyan
    } else if waiting {
        WAITING_ACTIVITY_COLOR
    } else {
        Color::DarkGray
    })
}

/// A staged tile's label, padded so it sits in the border's own gap
/// (`┌ title ┐`) instead of colliding with it (`┌title┐`), and filled so the
/// row reads as a header: reversed cyan when focused, slate (black on grey, no
/// modifier) otherwise. Every staged tile's label goes through here except one
/// being dragged, which `tile_block` draws in the gesture colour.
///
/// Grey, not a reversed default-colour tab, keeps the focused tab the
/// brightest block on screen. The measurement and the operator's report are in
/// `docs/REQUIREMENTS.md`, "The title row of a tile".
///
/// The text is never `DarkGray` and never dimmed. It is drawn over the border
/// row, so a label with no colour of its own inherited the unfocused border's
/// `DarkGray`, and dimming on top of that made it close to invisible on a
/// dark terminal theme (operator report, 2026-11-10). Named colours only (see
/// `border_style`, whose focus colour this reuses): they resolve through the
/// user's own terminal palette rather than a hard-coded truecolor pair.
fn pane_title(text: &str, focused: bool) -> Span<'static> {
    let padded = title_text(text);
    if focused {
        Span::styled(
            padded,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::REVERSED),
        )
    } else {
        Span::styled(padded, Style::default().fg(Color::Black).bg(Color::Gray))
    }
}

/// Enables bracketed paste (and mouse capture) on the HOST terminal. On
/// Unix backends this is what makes a multiline paste arrive as one
/// `Event::Paste` instead of a stream of individual key events. **On
/// Windows it has no such effect**: crossterm 0.29's Windows input backend
/// never constructs `Event::Paste` regardless of this call — `ReadConsoleInput`
/// maps every key press to one `Event::Key`, one at a time, with no code
/// path that ever batches a paste (source-confirmed; see `paste_accum`'s
/// module doc). `PasteAccumulator` exists to make up the difference: on
/// Windows a real paste, a drag-and-drop path, and an IME commit all arrive
/// here as an ordinary machine-pace key burst, and it re-derives "that was a
/// paste" from inter-key timing alone. This call is kept regardless — it
/// costs nothing on the platform where it doesn't help, and is what makes
/// Unix backends behave correctly.
fn setup_terminal() -> Result<(Tui, FrameOutputHandle<Stdout>)> {
    install_panic_hook();
    enable_raw_mode()?;
    // `run` returns on a setup error without reaching `restore_terminal`, so a
    // failure past this point undoes what was already done.
    enter_screen().inspect_err(|_| {
        let _ = restore_terminal();
    })
}

fn enter_screen() -> Result<(Tui, FrameOutputHandle<Stdout>)> {
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        EnableFocusChange
    )?;
    let timings = Rc::new(CellRef::new(BackendTimings::default()));
    let (writer, frame_output) = FrameWriter::new(stdout, timings.clone());
    Ok((
        Terminal::new(TimedBackend::new(CrosstermBackend::new(writer), timings)?)?,
        frame_output,
    ))
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    write_restore_sequence(&mut io::stdout())?;
    Ok(())
}

/// What undoes `setup_terminal`'s escape sequences, shared by the normal exit,
/// the panic hook and a failed setup so the lists cannot drift. `Show` comes
/// after `LeaveAlternateScreen`, so it lands on the main screen: ratatui hides
/// the cursor while drawing, and zsh does not re-show a hidden cursor at its
/// prompt.
fn write_restore_sequence(out: &mut impl io::Write) -> io::Result<()> {
    execute!(
        out,
        LeaveAlternateScreen,
        Show,
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange
    )
}

/// Open the diagnostic input-event log when `BANTO_INPUT_LOG` is set: one
/// `{ms} emporium: <message>` line per entry. Diagnostic plumbing only: never
/// read by anything in this crate, and `engine.rs` (the pure core) never sees
/// it.
fn open_input_log() -> Option<std::fs::File> {
    let path = std::env::var_os("BANTO_INPUT_LOG")?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

fn log_input(file: &mut Option<std::fs::File>, message: &str) {
    use std::io::Write as _;
    if let Some(file) = file {
        let ms = std::time::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(file, "{ms} emporium: {message}");
    }
}

/// An iteration's non-poll work is logged when its total exceeds this —
/// loose enough to skip the loop's own ordinary per-iteration cost, tight
/// enough to catch a stall well short of a `PASTE_GAP` (25ms).
const SLOW_ITERATION_THRESHOLD: Duration = Duration::from_millis(15);

/// Open the diagnostic stage-timing log when `BANTO_STAGE_LOG` is set — same
/// house pattern as [`open_input_log`] (and `BANTO_VERIFY_CACHE`,
/// `provider::cache`): env var names the file, append mode, best-effort
/// open. Unlike `BANTO_INPUT_LOG`, this one is never written to per key —
/// only per iteration whose stages summed past [`SLOW_ITERATION_THRESHOLD`]
/// — precisely so this instrument does not become a second version of the
/// self-reference risk `log_input` already carries (unbuffered synchronous
/// `File` I/O on this same thread, twice a key): logging only above
/// threshold means it cannot fire at burst rates the way `log_input` does.
fn open_stage_log() -> Option<std::fs::File> {
    let path = std::env::var_os("BANTO_STAGE_LOG")?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// Open the diagnostic bell log when `BANTO_BELL_LOG` is set: one line per bell
/// that was executed, saying what the core rang for. The same house pattern as
/// [`open_stage_log`]: the env var names the file, append mode, best-effort
/// open. A ring the core's cooldown swallowed is never a `Cmd` and so is not in
/// it.
///
/// Like the other diagnostic logs it is opened and written on the UI thread, so
/// the path should name a regular file: a FIFO with no reader would block the
/// event loop.
fn open_bell_log() -> Option<std::fs::File> {
    let path = std::env::var_os("BANTO_BELL_LOG")?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// One iteration's non-poll stage durations — the measuring (`Instant::now()`
/// deltas around each stage) is unconditional and negligible, no different
/// in cost from the loop's own existing per-iteration clock reads; only the
/// logging below is gated, both by `BANTO_STAGE_LOG` being set at all and by
/// the total exceeding [`SLOW_ITERATION_THRESHOLD`].
struct StageTimings {
    pty_pump: Duration,
    discovery: Duration,
    reload: Duration,
    tick: Duration,
    dispatch: Duration,
    draw: Duration,
}

impl StageTimings {
    fn total(&self) -> Duration {
        self.pty_pump + self.discovery + self.reload + self.tick + self.dispatch + self.draw
    }
}

/// Sub-stage durations inside the ~1s tick block — filled in by
/// `gather_session_moves`/`gather_relay_observations`/
/// `goinkyo_events` as they run, so `tick`'s own cost in
/// [`StageTimings`] is attributed rather than guessed at. The Goinkyo read runs
/// inside the dispatch loop but is counted into `tick`, not `dispatch`. Same measurement
/// discipline as [`StageTimings`]: accumulation is unconditional and cheap
/// (`Instant::now()` deltas), only the eventual log write is gated. Per-member
/// work inside each function's own loop accumulates into these fields as it
/// goes rather than logging per member — one line at the end either way,
/// same as everything else this file measures.
#[derive(Default)]
struct TickTimings {
    /// The brigade list and every roster, read once for the whole tick
    /// ([`TickRead`]).
    roster: Duration,
    /// The live-state directory scan, read once for the whole tick.
    live_scan: Duration,
    session_moves: Duration,
    /// The one shared `SysinfoProbe::alive_live_pids` pass every Claude
    /// member's activity check now reads from, instead of each paying its
    /// own — see that call site's own comment. Kept as its own field
    /// rather than folded into `relay_activity_claude` so this cost stays
    /// visible under its own name if it ever grows again, instead of
    /// silently vanishing into a line that got faster for an unrelated
    /// reason.
    relay_claude_snapshot: Duration,
    relay_unseen: Duration,
    /// The message-glance gate and the reads it lets through (`GlanceGate`).
    relay_glance: Duration,
    relay_activity_claude: Duration,
    relay_activity_codex_sqlite: Duration,
    relay_activity_codex_rollout: Duration,
    goinkyo_roster: Duration,
}

/// Which members' message glances have been read, and at which newest message
/// id, so a tick with no new message reads nothing. Lives in the event loop,
/// next to the other per-run shell state; the core never sees it (it keeps the
/// glances themselves, `engine::MemberFacts`).
///
/// Two answers are not gated. A member with unseen mail has its inbound glance
/// read every tick: the relay may nudge it in this very tick and quote that
/// mail, and any comparison of message ids leaves a window in which a message
/// arrives between the comparison and the read. (The sent glance of such a
/// member is still gated, from the copy kept here.) And with no newest id at
/// all — the lookup failed — everything is read.
///
/// Measured on a synthetic store of 6000 messages over 6 brigades: the two
/// reads for three members cost about 0.6 ms, and a member nobody has ever
/// written to or heard from about 0.4 ms on its own (no index on
/// `brigade_messages`, so the scan runs the whole table) — per tick, every
/// second. The gate is one 40 us rowid lookup.
#[derive(Default)]
struct GlanceGate {
    /// Per member: what was read for it last. The session is part of the
    /// identity: the Goinkyo's token is reused by each new consultation (a
    /// Worker's never is within its brigade, but a hand-edited store could), and
    /// a new member under an old token is a new member.
    read_at: HashMap<(BrigadeId, MemberToken), GlanceRead>,
    /// The members [`Self::read`] was asked about since [`Self::begin_tick`].
    visited: HashSet<(BrigadeId, MemberToken)>,
    /// What this tick's gather read about each brigade it visited (name and
    /// Director session); the loop takes it for the `Tick`. Lives here only
    /// because this is the one value `gather_relay_observations` already
    /// threads through for the tick's own memory.
    brigades: Vec<BrigadeNameFacts>,
    /// Whether each hosted pane is known to be idle, as this tick's gather read
    /// it, for [`Event::QuiescenceSampled`]. Complete for every hosted pane the
    /// gather could say anything about, whether or not the roster could be read.
    quiescence: Vec<(SessionKey, Quiescence)>,
}

/// What [`GlanceGate`] remembers of one member's last read.
struct GlanceRead {
    /// The newest message id of the tick it was read in — never newer than
    /// what the read could see, so a message that lands mid-read is read
    /// again next tick rather than skipped.
    newest_message: i64,
    session_id: String,
    /// The sent half, kept so a member whose inbound half is re-read every
    /// tick (it has unseen mail) does not also re-read this one.
    sent: Option<MessageGlance>,
}

/// Who a glance is read for.
struct GlanceMember<'a> {
    brigade_id: BrigadeId,
    token: &'a str,
    session_id: &'a str,
    role: BrigadeRole,
}

impl GlanceGate {
    /// Start a tick's visits (see [`Self::end_tick`]), and forget what the
    /// previous tick read about brigades.
    fn begin_tick(&mut self) {
        self.visited.clear();
        self.brigades.clear();
        self.quiescence.clear();
    }

    /// Drop every entry for a member this tick did not visit — dismissed,
    /// disbanded, or simply not live right now — so a resident process does
    /// not keep a record per member it has ever seen. A member that comes back
    /// is read again, which is the safe direction.
    fn end_tick(&mut self) {
        let visited = std::mem::take(&mut self.visited);
        self.read_at.retain(|id, _| visited.contains(id));
    }

    /// The member's glance if it needs reading: never read yet, a message has
    /// been appended since, or `has_unseen` (see the type's doc). `None` means
    /// "nothing new": the core keeps what it holds. A failed read also answers
    /// `None` and is retried next tick, so a transient error never blanks a
    /// tile.
    fn read(
        &mut self,
        store: &Store,
        member: GlanceMember<'_>,
        newest_message: Option<i64>,
        has_unseen: bool,
    ) -> Option<MemberGlance> {
        let GlanceMember {
            brigade_id,
            token,
            session_id,
            role,
        } = member;
        let id = (brigade_id, token.to_string());
        self.visited.insert(id.clone());
        let known_sent = match (newest_message, self.read_at.get(&id)) {
            (Some(newest), Some(read))
                if read.newest_message == newest && read.session_id == session_id =>
            {
                Some(read.sent.clone())
            }
            _ => None,
        };
        if known_sent.is_some() && !has_unseen {
            return None;
        }
        let inbound = store.latest_inbound_glance(brigade_id, token, role).ok()?;
        let sent = match known_sent {
            Some(sent) => sent,
            None => store.latest_sent_glance(brigade_id, token).ok()?,
        };
        if let Some(newest_message) = newest_message {
            self.read_at.insert(
                id,
                GlanceRead {
                    newest_message,
                    session_id: session_id.to_string(),
                    sent: sent.clone(),
                },
            );
        }
        Some(MemberGlance { inbound, sent })
    }
}

#[derive(Default)]
struct DrawTimings {
    assemble: Duration,
    hyperlink_close: Duration,
    hyperlink_bytes: u64,
    skipped: bool,
    backend: BackendTimings,
}

/// The last output sent to the host, including the cursor request which is
/// intentionally outside ratatui's public `Buffer` API. A matching frame can
/// therefore avoid every backend operation, not merely an empty cell diff.
struct PresentedFrame {
    buffer: Buffer,
    cursor: Option<Position>,
}

fn should_present(
    presented: Option<&PresentedFrame>,
    buffer: &Buffer,
    cursor: Option<Position>,
) -> bool {
    presented.is_none_or(|previous| previous.buffer != *buffer || previous.cursor != cursor)
}

/// Stage name and duration only — no keystroke content, no paths, no
/// payload, the same discretion `banto_io::provider::cache`'s own
/// verification log already exercises (path-only on divergence, never
/// parsed content).
///
/// A second line, breaking `tick` down into `TickTimings`' fields, is
/// emitted only when `tick` is the largest of the six `StageTimings`
/// stages this iteration — not unconditionally alongside the first line.
/// `tick` only ever fires ~once a second (`TICK_INTERVAL`), so on most
/// slow iterations it is near zero and a breakdown of it would be pure
/// noise crowding out whichever stage actually was slow; folding the ten
/// extra fields into the first line unconditionally would do the same
/// thing to every line, including the ones where `reload` or `draw` is the
/// story. A second, conditional line keeps both readable.
fn log_slow_iteration(
    file: &mut Option<std::fs::File>,
    timings: &StageTimings,
    tick_timings: &TickTimings,
    draw_timings: &DrawTimings,
) {
    use std::io::Write as _;
    let total = timings.total();
    if total < SLOW_ITERATION_THRESHOLD {
        return;
    }
    let Some(file) = file else {
        return;
    };
    let ms = std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = writeln!(
        file,
        "{ms} emporium: slow iter total={}us pty_pump={}us discovery={}us reload={}us tick={}us dispatch={}us draw={}us",
        total.as_micros(),
        timings.pty_pump.as_micros(),
        timings.discovery.as_micros(),
        timings.reload.as_micros(),
        timings.tick.as_micros(),
        timings.dispatch.as_micros(),
        timings.draw.as_micros(),
    );
    let tick_is_dominant = [
        timings.pty_pump,
        timings.discovery,
        timings.reload,
        timings.dispatch,
        timings.draw,
    ]
    .into_iter()
    .all(|other| timings.tick >= other);
    if tick_is_dominant {
        let _ = writeln!(
            file,
            "{ms} emporium: slow tick roster={}us live_scan={}us session_moves={}us \
             relay_claude_snapshot={}us relay_unseen={}us \
             relay_glance={}us relay_activity_claude={}us relay_activity_codex_sqlite={}us \
             relay_activity_codex_rollout={}us goinkyo_roster={}us",
            tick_timings.roster.as_micros(),
            tick_timings.live_scan.as_micros(),
            tick_timings.session_moves.as_micros(),
            tick_timings.relay_claude_snapshot.as_micros(),
            tick_timings.relay_unseen.as_micros(),
            tick_timings.relay_glance.as_micros(),
            tick_timings.relay_activity_claude.as_micros(),
            tick_timings.relay_activity_codex_sqlite.as_micros(),
            tick_timings.relay_activity_codex_rollout.as_micros(),
            tick_timings.goinkyo_roster.as_micros(),
        );
    }
    let draw_is_dominant = [
        timings.pty_pump,
        timings.discovery,
        timings.reload,
        timings.tick,
        timings.dispatch,
    ]
    .into_iter()
    .all(|other| timings.draw >= other);
    if draw_is_dominant {
        let backend = &draw_timings.backend;
        // The OSC 8 close now travels through the same frame writer, so its
        // bytes are already included in the backend total (unlike the old
        // direct stdout close, which needed adding separately).
        let bytes = backend.bytes_written;
        let _ = writeln!(
            file,
            "{ms} emporium: slow draw skipped={} assemble={}us hyperlink_close={}us hyperlink_bytes={} \
             bytes={} backend_draw={}us/{} append_lines={}us/{} hide_cursor={}us/{} \
             show_cursor={}us/{} get_cursor_position={}us/{} set_cursor_position={}us/{} \
             clear={}us/{} clear_region={}us/{} size={}us/{} window_size={}us/{} flush={}us/{} \
             present_write={}us/{} present_flush={}us/{}",
            draw_timings.skipped,
            draw_timings.assemble.as_micros(),
            draw_timings.hyperlink_close.as_micros(),
            draw_timings.hyperlink_bytes,
            bytes,
            backend.draw.elapsed.as_micros(),
            backend.draw.calls,
            backend.append_lines.elapsed.as_micros(),
            backend.append_lines.calls,
            backend.hide_cursor.elapsed.as_micros(),
            backend.hide_cursor.calls,
            backend.show_cursor.elapsed.as_micros(),
            backend.show_cursor.calls,
            backend.get_cursor_position.elapsed.as_micros(),
            backend.get_cursor_position.calls,
            backend.set_cursor_position.elapsed.as_micros(),
            backend.set_cursor_position.calls,
            backend.clear.elapsed.as_micros(),
            backend.clear.calls,
            backend.clear_region.elapsed.as_micros(),
            backend.clear_region.calls,
            backend.size.elapsed.as_micros(),
            backend.size.calls,
            backend.window_size.elapsed.as_micros(),
            backend.window_size.calls,
            backend.flush.elapsed.as_micros(),
            backend.flush.calls,
            backend.present_write.elapsed.as_micros(),
            backend.present_write.calls,
            backend.present_flush.elapsed.as_micros(),
            backend.present_flush.calls,
        );
    }
}

/// Env-gated recorder for `docs/DISCIPLINE.md` §8's record/replay stream:
/// every `Event` fed into `engine::update`, one JSON line per event
/// (`banto_core::replay::TimedEvent`), `offset_ms` relative to when this
/// run's event loop started. A `banto_io`/shell concern under §6.2's
/// diagnostic-bypass relaxation — `banto_core::replay` only ever reads this
/// format back, never writes it.
///
/// **The resulting file is a LOCAL DIAGNOSTIC ARTIFACT, never a repo
/// fixture.** Unlike `BANTO_INPUT_LOG` (which deliberately logs a paste's
/// length, never its text), this recorder writes every event whole —
/// keystrokes, pasted text, PTY output chunks, session ids, and — in every
/// relay `Tick` — the first line of each live member's newest brigade
/// messages (what a collapsed tile shows; `RelayObservation::glance`). A real
/// capture necessarily contains real session content; repo invariant 2 ("never
/// bring real session data into the repository") applies with full force.
/// `banto_core::replay`'s own fixtures are hand-written synthetic streams
/// only — never a `BANTO_RECORD_EVENTS` capture, however tempting it is to
/// just point it at a real run and commit the result.
struct EventRecorder {
    file: std::fs::File,
    run_start: Instant,
}

impl EventRecorder {
    /// Open (creating if needed, appending if not) the recorder at `path`,
    /// writing the version header only when the file is new or empty — so
    /// re-running with the same path keeps appending to one stream instead
    /// of writing a header mid-file. Separated from [`open_event_recorder`]
    /// (which resolves `path` from `BANTO_RECORD_EVENTS`) so the actual
    /// header/append mechanics are testable against a real temp file
    /// without mutating process-global environment state.
    fn open(path: &std::path::Path, run_start: Instant) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let is_new = file.metadata()?.len() == 0;
        let mut recorder = Self { file, run_start };
        if is_new {
            recorder.write_header();
        }
        Ok(recorder)
    }

    fn write_header(&mut self) {
        use std::io::Write as _;
        let _ = writeln!(self.file, "{{\"banto_event_stream\":{STREAM_VERSION}}}");
    }

    /// Append one event at `now`, as an offset from `run_start`. Silent on
    /// any failure — JSON serialization or the write itself — matching
    /// `log_input`'s stance: a diagnostics channel must never take down the
    /// TUI.
    fn record(&mut self, event: &Event, now: Instant) {
        use std::io::Write as _;
        let offset_ms = now.saturating_duration_since(self.run_start).as_millis() as u64;
        let timed = TimedEvent {
            offset_ms,
            event: event.clone(),
        };
        if let Ok(line) = serde_json::to_string(&timed) {
            let _ = writeln!(self.file, "{line}");
        }
    }
}

/// Open the event recorder when `BANTO_RECORD_EVENTS` is set — see
/// [`EventRecorder`]'s doc for what it captures and the loud warning that
/// pairs with it.
fn open_event_recorder(run_start: Instant) -> Option<EventRecorder> {
    let path = std::env::var_os("BANTO_RECORD_EVENTS")?;
    EventRecorder::open(std::path::Path::new(&path), run_start).ok()
}

/// Release one event `paste_acc` flushed: logs the synthesis (length and
/// `Enter` count only, never the text — same privacy stance as
/// [`describe_raw_event`]/[`describe_converted_event`]) when it turns out
/// to be a synthesized paste, then queues it exactly like any other input
/// event. A lone released key (the common case: nothing needed joining)
/// gets no extra log line — it was already logged as a "converted key ..."
/// at read time, before scope classification ever ran.
fn emit_flushed(
    events: &mut VecDeque<Event>,
    input_log: &mut Option<std::fs::File>,
    flushed: InputEvent,
) {
    if let InputEvent::Paste(text) = &flushed {
        let enters = text.matches('\r').count();
        log_input(
            input_log,
            &format!("paste synthesized len={} enters={enters}", text.len()),
        );
    }
    events.push_back(Event::Input(flushed));
}

/// The `Event` a raw `FocusGained`/`FocusLost` becomes, or `None` for every
/// other crossterm event kind — the main loop's own translation step,
/// pulled out as a pure function specifically so this mapping is checkable
/// without a live terminal: `FocusGained`/`FocusLost` are not keys and never
/// reach `convert::from_crossterm` at all (see the main loop's own call
/// site), so nothing else in this file exercises this translation.
fn window_focus_event(raw: &crossterm::event::Event) -> Option<Event> {
    match raw {
        crossterm::event::Event::FocusGained => Some(Event::WindowFocusChanged { focused: true }),
        crossterm::event::Event::FocusLost => Some(Event::WindowFocusChanged { focused: false }),
        _ => None,
    }
}

/// The brigade's `interrupt_worker` requests no outcome is recorded for, with
/// their age by the wall clock (the core has none, and an `Instant` would not
/// survive a restart, which is the very case the age exists for). `None` when the
/// store could not be read, which the core must tell apart from an empty list:
/// nothing is decided on a tick that could not read and nothing already decided
/// is forgotten, and the request is still there for the next.
///
/// `read` is as the store had it a moment ago. A Worker can pull the message
/// between this read and the press that follows; that window is accepted, and the
/// price is one Esc that was not needed.
fn interrupt_facts(
    store: &Store,
    brigade_id: BrigadeId,
) -> Option<Vec<banto_core::engine::InterruptFact>> {
    let Ok(pending) = store.pending_interrupts(brigade_id) else {
        return None;
    };
    let now_ms = std::time::UNIX_EPOCH
        .elapsed()
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    Some(
        pending
            .into_iter()
            .map(|request| banto_core::engine::InterruptFact {
                message_id: request.message_id,
                member: request.member,
                age_ms: u64::try_from(now_ms.saturating_sub(request.created_at_ms)).unwrap_or(0),
                read: request.read,
            })
            .collect(),
    )
}

fn pump_pty_handles(handles: &mut HashMap<SessionKey, PtyHandle>, events: &mut VecDeque<Event>) {
    for (key, handle) in handles {
        // Whatever an interrupting Esc held back leaves on the first turn of the
        // loop after its quiet time, not on the next unrelated write.
        handle.flush_quiet();
        loop {
            match handle.poll() {
                PtyPoll::Chunk(chunk) => events.push_back(Event::PtyOutput {
                    key: key.clone(),
                    chunk,
                }),
                PtyPoll::Empty => break,
                PtyPoll::OutputLost => {
                    events.push_back(Event::PtyOutputLost { key: key.clone() });
                    break;
                }
                PtyPoll::Disconnected => {
                    events.push_back(super::session::disconnect_event(
                        key.clone(),
                        handle.exit_reason(),
                    ));
                    break;
                }
            }
        }
    }
}

fn reap_pty_handles(
    handles: &mut HashMap<SessionKey, PtyHandle>,
    state: &EmporiumState,
) -> Vec<(SessionKey, PtyHandle)> {
    handles
        .extract_if(|key, handle| {
            !state.screens.contains_key(key) && !handle.awaiting_exit_after_output_loss()
        })
        .collect()
}

/// Compact description of one raw crossterm event, logged before conversion.
/// A paste is logged as its length, never its text; a key is logged as its code,
/// kind and modifiers, so the log holds the operator's keystrokes — typed
/// characters included — and is not something to paste into a bug report
/// unread.
fn describe_raw_event(event: &crossterm::event::Event) -> String {
    match event {
        crossterm::event::Event::Key(key) => format!(
            "raw key code={:?} kind={:?} mods={:?}",
            key.code, key.kind, key.modifiers
        ),
        crossterm::event::Event::Mouse(mouse) => format!(
            "raw mouse kind={:?} col={} row={}",
            mouse.kind, mouse.column, mouse.row
        ),
        crossterm::event::Event::Paste(text) => format!("raw paste len={}", text.len()),
        crossterm::event::Event::Resize(width, height) => {
            format!("raw resize {width}x{height}")
        }
        crossterm::event::Event::FocusGained => "raw focus_gained".to_string(),
        crossterm::event::Event::FocusLost => "raw focus_lost".to_string(),
    }
}

/// Compact description of the `InputEvent` a raw event converted into —
/// paired with [`describe_raw_event`] so one operator paste attempt yields a
/// definitive "what crossterm handed us" vs. "what banto understood it as"
/// capture, in one file, one line each.
fn describe_converted_event(event: &InputEvent) -> String {
    match event {
        InputEvent::Key(key) => {
            format!("converted key code={:?} mods={:?}", key.code, key.modifiers)
        }
        InputEvent::Mouse(mouse) => format!(
            "converted mouse kind={:?} col={} row={}",
            mouse.kind, mouse.column, mouse.row
        ),
        InputEvent::Paste(text) => format!("converted paste len={}", text.len()),
        InputEvent::Resize { width, height } => format!("converted resize {width}x{height}"),
    }
}

/// Deliberately does NOT run `shutdown_handles`: this hook is installed once
/// in `setup_terminal`, before `handles` (owned locally by `event_loop`)
/// even exists, and a `'static` panic hook has no clean way to reach a
/// stack-local `HashMap<SessionKey, PtyHandle>` that may itself be
/// mid-mutation at panic time without new shared mutable state — which
/// would be a poor trade for a path that only exists to leave the terminal
/// sane before re-raising. Left as terminal-restore-only; graceful PTY
/// teardown on panic is not attempted.
fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = write_restore_sequence(&mut io::stdout());
            original(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use banto_core::engine::Stage;
    use banto_core::model::{Activity, AgentKind, SessionRow};
    use banto_io::input::mock::MockInputReader;
    use banto_io::pty::mock::MockPtyHost;
    use banto_io::status::ProcessProbe;

    use ratatui::widgets::Widget as _;

    use super::*;

    /// A Codex that is not there: what every test of the emporium that is not
    /// about the catalog gets, so none of them can run the real one.
    struct NoCatalog;

    impl CodexCatalogSource for NoCatalog {
        fn bundled_catalog(&self, _binary: &str) -> Result<String, String> {
            Err("no Codex in tests".to_string())
        }
    }

    /// A Codex that answers from a script and records which binary it was asked
    /// for.
    struct ScriptedCatalog {
        answer: Result<String, String>,
        asked: RefCell<Vec<String>>,
    }

    impl CodexCatalogSource for ScriptedCatalog {
        fn bundled_catalog(&self, binary: &str) -> Result<String, String> {
            self.asked.borrow_mut().push(binary.to_string());
            self.answer.clone()
        }
    }

    #[test]
    fn the_drag_line_replaces_what_the_frame_had_under_it_and_nothing_beside_it() {
        let area = Rect::new(0, 0, 6, 4);
        let mut buffer = Buffer::empty(area);
        for y in 0..4 {
            for x in 0..6 {
                buffer[(x, y)].set_symbol("x").set_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
            }
        }

        paint_drag_line(&mut buffer, Rect::new(2, 0, 1, 4), Axis::Vertical);
        paint_drag_line(&mut buffer, Rect::new(0, 3, 6, 1), Axis::Horizontal);

        for y in 0..3 {
            let cell = &buffer[(2, y)];
            assert_eq!(cell.symbol(), "\u{2503}");
            assert_eq!(cell.fg, DRAG_LINE_COLOR);
            assert!(
                cell.modifier.is_empty(),
                "the border's bold does not leak in"
            );
            assert_eq!(buffer[(3, y)].symbol(), "x");
        }
        // The horizontal line wins where the two cross: drawn last.
        assert_eq!(buffer[(2, 3)].symbol(), "\u{2501}");
        assert_eq!(buffer[(0, 3)].symbol(), "\u{2501}");
    }

    #[test]
    fn the_cells_core_calls_the_label_are_the_cells_the_block_draws_it_on() {
        // `banto_core::layout::title_columns` decides which top-row cells the
        // stack boundary's handle keeps off; this renders the real title and
        // finds where it actually landed, for short, exact-fit and clipped
        // labels and a tile too narrow for any.
        for (width, label) in [
            (40u16, "worker-2"),
            (20, "worker-2"),
            (20, "an-extremely-long-member-label"),
            (12, "director"),
            (3, "x"),
            (2, "x"),
        ] {
            let tile = Rect::new(5, 2, width, 4);
            let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 8));
            Block::bordered()
                .title(pane_title(label, false))
                .render(tile, &mut buffer);
            let (start, end) = banto_core::layout::title_columns(tile, label);
            for x in tile.x + 1..tile.x + width.saturating_sub(1) {
                let is_border = buffer[(x, tile.y)].symbol() == "\u{2500}";
                assert_eq!(
                    !is_border,
                    (start..end).contains(&x),
                    "width {width}, {label:?}, column {x}"
                );
            }
        }
    }

    /// A brigade of the Director and two Workers staged through the core's
    /// own formation events, on a 120x40 terminal.
    fn staged_brigade_of_two_workers() -> (EmporiumState, App, BrigadeConfig) {
        staged_brigade_of_two_workers_with_rows(Vec::new())
    }

    /// The same, with `extra_rows` in the session list (to give the Workers
    /// rows of their own: the Worker panes are keyed
    /// `new-worker::1::worker-N` until the store learns their session ids).
    fn staged_brigade_of_two_workers_with_rows(
        extra_rows: Vec<SessionRow>,
    ) -> (EmporiumState, App, BrigadeConfig) {
        staged_brigade_of_workers_with_rows(2, extra_rows)
    }

    fn staged_brigade_of_workers_with_rows(
        workers: usize,
        extra_rows: Vec<SessionRow>,
    ) -> (EmporiumState, App, BrigadeConfig) {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut rows = vec![test_row("dir", AgentKind::ClaudeCode)];
        rows.extend(extra_rows);
        let mut app = App::new(rows);
        let brigade = BrigadeConfig::default();
        let now = Instant::now();
        let cmds = engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::BrigadeFormed {
                director_row_id: "dir".to_string(),
                name: "cell".to_string(),
                cwd: PathBuf::from("/work"),
                worker_agent: AgentKind::ClaudeCode,
                worker_model: "sonnet".to_string(),
                worker_effort: "high".to_string(),
                result: Ok((1, (1..=workers).map(|n| format!("worker-{n}")).collect())),
            },
            now,
        );
        let director = match cmds.as_slice() {
            [Cmd::OpenEmbedded { key, .. }] => key.clone(),
            other => panic!("expected a Director open, got {other:?}"),
        };
        let mut opened = vec![director.clone()];
        let cmds = engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: director,
                agent: None,
            },
            now,
        );
        opened.extend(cmds.iter().filter_map(|cmd| match cmd {
            Cmd::OpenEmbedded { key, .. } => Some(key.clone()),
            _ => None,
        }));
        for key in opened.into_iter().skip(1) {
            engine::update(
                &mut state,
                &mut app,
                &brigade,
                Event::Spawned { key, agent: None },
                now,
            );
        }
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 120,
                height: 40,
            },
            now,
        );
        (state, app, brigade)
    }

    fn mouse(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        kind: banto_core::input::MouseEventKind,
        column: u16,
        row: u16,
    ) {
        engine::update(
            state,
            app,
            brigade,
            Event::Input(InputEvent::Mouse(banto_core::input::MouseEvent {
                kind,
                column,
                row,
            })),
            Instant::now(),
        );
    }

    fn render(state: &EmporiumState, app: &App) -> Buffer {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        let mut cache = HashMap::new();
        terminal
            .draw(|frame| {
                let _ = draw(
                    frame,
                    app,
                    state,
                    SystemTime::now(),
                    Instant::now(),
                    &mut cache,
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn text_at(buffer: &Buffer, x: u16, y: u16, len: u16) -> String {
        (x..x + len).map(|x| buffer[(x, y)].symbol()).collect()
    }

    // --- the idle hint line follows the focus -----------------------------------

    fn status_row(buffer: &Buffer) -> String {
        let bottom = buffer.area.height - 1;
        text_at(buffer, 0, bottom, buffer.area.width)
    }

    /// Feed `event` to the core at `now` and draw the result's bottom line.
    fn status_after(
        state: &mut EmporiumState,
        app: &mut App,
        event: Event,
        now: Instant,
    ) -> String {
        engine::update(state, app, &BrigadeConfig::default(), event, now);
        status_row(&render(state, app))
    }

    #[test]
    fn startup_notices_reach_the_status_bar_one_after_the_other_and_then_the_hints_return() {
        let mut app = App::new(Vec::new());
        let mut state = EmporiumState::new(PrefixKey::default());
        let t0 = Instant::now();
        let notices = vec![
            "codex: brigade briefings need a one-time approval".to_string(),
            "data_dir: the database is not in it".to_string(),
        ];

        // Both apply: the first is up at once and the second is not lost, only
        // waiting for the line.
        let first = status_after(&mut state, &mut app, Event::StartupNotices { notices }, t0);
        assert!(first.contains("one-time approval"), "{first:?}");
        assert!(!first.contains("data_dir"), "{first:?}");

        // The first has had its time: the second takes the line on that tick.
        let tick = || Event::Tick {
            relay: Vec::new(),
            brigades: Vec::new(),
        };
        let second = status_after(&mut state, &mut app, tick(), t0 + Duration::from_secs(6));
        assert!(second.contains("the database is not in it"), "{second:?}");
        assert!(!second.contains("one-time approval"), "{second:?}");

        // And when that has run out too, the key guidance is back.
        let after = status_after(&mut state, &mut app, tick(), t0 + Duration::from_secs(12));
        assert!(after.contains("j/k move"), "{after:?}");
    }

    #[test]
    fn with_no_startup_notice_the_status_bar_shows_the_hints() {
        let mut app = App::new(Vec::new());
        let mut state = EmporiumState::new(PrefixKey::default());
        let line = status_after(
            &mut state,
            &mut app,
            Event::StartupNotices {
                notices: Vec::new(),
            },
            Instant::now(),
        );
        assert!(line.contains("j/k move"), "{line:?}");
    }

    // --- quitting: the prompt and the closing line ---------------------------------

    /// `hosted` plain sessions with screens, the list focused, and the bar drawn
    /// once so `previous_ring_set` and friends are seeded.
    fn quitting_fixture(hosted: usize) -> (EmporiumState, App, Instant) {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = App::new(Vec::new());
        let at = Instant::now();
        engine::update(
            &mut state,
            &mut app,
            &BrigadeConfig::default(),
            Event::Resized {
                width: 120,
                height: 40,
            },
            at,
        );
        for i in 0..hosted {
            state
                .screens
                .insert(SessionKey::from_id(&format!("s{i}")), Screen::new(24, 80));
        }
        state.focus = Focus::Sidebar;
        (state, app, at)
    }

    fn press_esc(state: &mut EmporiumState, app: &mut App, at: Instant) -> String {
        status_after(
            state,
            app,
            key_event(
                banto_core::input::KeyCode::Esc,
                banto_core::input::Modifiers::NONE,
            ),
            at,
        )
    }

    #[test]
    fn the_status_bar_shows_the_quit_prompt_while_it_is_armed() {
        let (mut state, mut app, at) = quitting_fixture(2);
        let bar = press_esc(&mut state, &mut app, at);
        assert!(
            bar.contains("Esc again to quit \u{2014} 2 not idle"),
            "{bar:?}"
        );
        assert!(!bar.contains("Closing"), "{bar:?}");
        assert!(!app.should_quit());
    }

    /// A list as long as a real one: the counts segment is `[999/9999]`, the widest
    /// a status bar is likely to carry, and what the quit question gets is what
    /// is left of the line. The three hosted sessions' rows come first and do not
    /// match the query the list is left filtered by; 999 fillers do.
    fn crowd_the_list(app: &mut App, titles: [&str; 3]) {
        let row = |id: String, title: String| banto_core::model::SessionRow {
            id,
            agent: AgentKind::ClaudeCode,
            title: Some(title),
            cwd: None,
            dialog_open: false,
            activity: banto_core::model::Activity::Alive,
            is_agent: false,
            preview: None,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
            archived: false,
        };
        let mut rows: Vec<_> = titles
            .iter()
            .enumerate()
            .map(|(i, title)| row(format!("s{i}"), title.to_string()))
            .collect();
        rows.extend((0..999).map(|n| row(format!("m-{n}"), format!("match {n}"))));
        rows.extend((0..8997).map(|n| row(format!("o-{n}"), format!("other {n}"))));
        app.replace_rows(rows);
        app.enter_search();
        "match".chars().for_each(|c| app.push_char(c));
        app.confirm_search();
    }

    /// The status row of the quit question armed over three hosted sessions read
    /// as `reads`, titled `titles` in the list, drawn at `width` columns, with the
    /// bell badge on or off.
    fn quit_question_at(
        width: u16,
        bell: bool,
        titles: [&str; 3],
        reads: [Quiescence; 3],
    ) -> String {
        let (mut state, mut app, at) = quitting_fixture(3);
        crowd_the_list(&mut app, titles);
        if bell {
            engine::update(
                &mut state,
                &mut app,
                &BrigadeConfig::default(),
                bell_switch(true),
                at,
            );
        }
        engine::update(
            &mut state,
            &mut app,
            &BrigadeConfig::default(),
            Event::QuiescenceSampled {
                samples: reads
                    .iter()
                    .enumerate()
                    .map(|(i, read)| (SessionKey::from_id(&format!("s{i}")), *read))
                    .collect(),
            },
            at,
        );
        press_esc(&mut state, &mut app, at);
        assert!(!app.should_quit());
        let bar = status_row_at_width(&state, &app, width);
        assert!(bar.contains("[999/9999]"), "{bar:?}");
        assert_eq!(bar.contains(banto_tui::hints::BELL_BADGE), bell, "{bar:?}");
        bar
    }

    const WIDE: &str = "a title that is much longer than the budget";

    #[test]
    fn at_80_columns_the_first_entry_of_each_quit_form_is_whole() {
        // The status line does not wrap, so what does not fit is cut off its end
        // and only the first entry is sure to be seen. Each form, drawn beside
        // the counts a long list has, shows it with its reason and any suffix.
        let [unknown, busy, background, idle] = [
            Quiescence::Unknown,
            Quiescence::NotIdle,
            Quiescence::Background,
            Quiescence::Idle,
        ];
        let cases = [
            // Not idle alone.
            (
                [WIDE, "b", "c"],
                [unknown, idle, idle],
                "Esc again to quit \u{2014} 1 not idle: a title th\u{2026} (unknown)",
            ),
            // Background alone: no reason, no tag.
            (
                [WIDE, "b", "c"],
                [background, idle, idle],
                "Esc again to quit \u{2014} 1 in background: a title th\u{2026}",
            ),
            // Both, the longest head there is, with the widest first entry.
            (
                [WIDE, "b", "c"],
                [unknown, busy, background],
                "Esc again to quit \u{2014} 2 not idle, 1 in background: \
                 a title th\u{2026} (unknown)",
            ),
            // Both again, the first entry told apart from the next by a suffix
            // that is paid for out of its name.
            (
                [WIDE, WIDE, "c"],
                [unknown, busy, background],
                "Esc again to quit \u{2014} 2 not idle, 1 in background: \
                 a titl\u{2026} #s0 (unknown)",
            ),
        ];
        for (titles, reads, expected) in cases {
            let bar = quit_question_at(80, false, titles, reads);
            assert!(bar.contains(expected), "{expected:?} not whole in {bar:?}");
        }
    }

    #[test]
    fn at_80_columns_with_the_bell_badge_a_single_group_question_still_shows_its_first_entry() {
        // The badge takes 12 columns from the question's side of the line. The
        // mixed form's longest head and first entry no longer fit beside it, and
        // nothing here promises they do.
        let [unknown, idle, background] = [
            Quiescence::Unknown,
            Quiescence::Idle,
            Quiescence::Background,
        ];
        let cases = [
            (
                [WIDE, "b", "c"],
                [unknown, idle, idle],
                "Esc again to quit \u{2014} 1 not idle: a title th\u{2026} (unknown)",
            ),
            (
                [WIDE, WIDE, "c"],
                [unknown, unknown, idle],
                "Esc again to quit \u{2014} 2 not idle: a titl\u{2026} #s0 (unknown)",
            ),
            (
                [WIDE, "b", "c"],
                [background, idle, idle],
                "Esc again to quit \u{2014} 1 in background: a title th\u{2026}",
            ),
        ];
        for (titles, reads, expected) in cases {
            let bar = quit_question_at(80, true, titles, reads);
            assert!(bar.contains(expected), "{expected:?} not whole in {bar:?}");
        }
    }

    #[test]
    fn the_status_bar_says_closing_with_the_count_once_quitting_is_decided() {
        let (mut state, mut app, at) = quitting_fixture(2);
        press_esc(&mut state, &mut app, at);
        let bar = press_esc(&mut state, &mut app, at + Duration::from_secs(1));
        assert!(app.should_quit());
        assert!(
            bar.contains("Closing \u{2014} ending 2 session(s)\u{2026}"),
            "{bar:?}"
        );
        assert!(!bar.contains("Esc again"), "the prompt is gone: {bar:?}");
        assert!(!bar.contains("j/k move"), "so are the hints: {bar:?}");
    }

    #[test]
    fn with_nothing_hosted_the_closing_line_is_the_bare_message() {
        let (mut state, mut app, at) = quitting_fixture(0);
        let bar = press_esc(&mut state, &mut app, at);
        assert!(app.should_quit(), "one press quits when nothing is hosted");
        assert!(bar.contains("Closing\u{2026}"), "{bar:?}");
        assert!(!bar.contains("ending"), "{bar:?}");
    }

    #[test]
    fn idle_sessions_close_on_the_first_press_and_the_line_counts_them_all() {
        let (mut state, mut app, at) = quitting_fixture(2);
        engine::update(
            &mut state,
            &mut app,
            &BrigadeConfig::default(),
            Event::QuiescenceSampled {
                samples: vec![
                    (SessionKey::from_id("s0"), Quiescence::Idle),
                    (SessionKey::from_id("s1"), Quiescence::Idle),
                ],
            },
            at,
        );
        let bar = press_esc(&mut state, &mut app, at);
        assert!(app.should_quit());
        assert!(
            bar.contains("Closing \u{2014} ending 2 session(s)\u{2026}"),
            "{bar:?}"
        );
    }

    #[test]
    fn the_closing_line_wins_over_every_other_thing_the_bar_can_show() {
        let (mut state, mut app, at) = quitting_fixture(1);
        press_esc(&mut state, &mut app, at);
        let bar = press_esc(&mut state, &mut app, at + Duration::from_secs(1));
        assert!(app.should_quit());
        assert!(bar.contains("Closing"), "{bar:?}");

        // Things that arrive once quitting is decided (a session that ends
        // while the sweep runs, a notice) and a prefix chord armed on top: the
        // status would show and the prefix hint would normally outrank it.
        let notice = Event::StartupNotices {
            notices: vec!["data_dir: the database is not in it".to_string()],
        };
        let bar = status_after(&mut state, &mut app, notice, at + Duration::from_secs(2));
        assert!(bar.contains("Closing"), "{bar:?}");
        assert!(!bar.contains("data_dir"), "{bar:?}");
        state.prefix_armed = Some(at);
        let bar = status_row(&render(&state, &app));
        assert!(bar.contains("Closing"), "{bar:?}");
        assert!(!bar.contains("prefix"), "{bar:?}");
    }

    #[test]
    fn without_a_quit_the_same_competitors_do_show() {
        // The counterpart of the test above: the line is only taken over once
        // quitting is decided.
        let (mut state, mut app, at) = quitting_fixture(1);
        let notice = Event::StartupNotices {
            notices: vec!["data_dir: the database is not in it".to_string()],
        };
        let bar = status_after(&mut state, &mut app, notice, at);
        assert!(bar.contains("data_dir"), "{bar:?}");
        assert!(!bar.contains("Closing"), "{bar:?}");
        state.prefix_armed = Some(at);
        let bar = status_row(&render(&state, &app));
        assert!(!bar.contains("Closing"), "{bar:?}");
    }

    #[test]
    fn the_closing_frame_is_never_equal_to_the_frame_before_it() {
        // The loop forces the final frame out regardless; this is the other
        // half of the guarantee: the comparison would not have skipped it
        // either, because the line it carries is new.
        let (mut state, mut app, at) = quitting_fixture(1);
        press_esc(&mut state, &mut app, at);
        let armed = render(&state, &app);
        press_esc(&mut state, &mut app, at + Duration::from_secs(1));
        let closing = render(&state, &app);
        let previous = PresentedFrame {
            buffer: armed,
            cursor: None,
        };
        assert!(should_present(Some(&previous), &closing, None));
    }

    // --- the brigade-bell switch: its badge and its persistence ---------------------

    fn bell_switch(on: bool) -> Event {
        Event::BellWhenFocusedLoaded { on }
    }

    #[test]
    fn the_badge_is_on_the_bar_in_list_focus_and_in_pane_focus_while_the_switch_is_on() {
        for focus in [Focus::Sidebar, Focus::Pane] {
            let (mut state, mut app, at) = quitting_fixture(0);
            state.focus = focus;
            let bar = status_after(&mut state, &mut app, bell_switch(true), at);
            assert!(
                bar.contains(banto_tui::hints::BELL_BADGE),
                "{focus:?}: {bar:?}"
            );
            // Right-aligned beside the counts, in the segment the left-hand
            // text never uses.
            assert!(
                bar.trim_end()
                    .ends_with(&format!("{} [0/0]", banto_tui::hints::BELL_BADGE)),
                "{bar:?}"
            );
        }
    }

    #[test]
    fn while_the_switch_is_off_the_bar_says_nothing_about_it() {
        for focus in [Focus::Sidebar, Focus::Pane] {
            let (mut state, mut app, at) = quitting_fixture(0);
            state.focus = focus;
            let bar = status_after(&mut state, &mut app, bell_switch(false), at);
            assert!(!bar.contains("BELL"), "{focus:?}: {bar:?}");
            assert!(!bar.to_lowercase().contains("off"), "{bar:?}");
        }
    }

    #[test]
    fn transient_text_cannot_displace_the_badge() {
        let (mut state, mut app, at) = quitting_fixture(1);
        status_after(&mut state, &mut app, bell_switch(true), at);
        // A status message, the armed prefix hint, the quit prompt and the
        // closing line each take the left-hand text.
        let notice = Event::StartupNotices {
            notices: vec!["data_dir: the database is not in it".to_string()],
        };
        let bar = status_after(&mut state, &mut app, notice, at);
        assert!(
            bar.contains("data_dir") && bar.contains("BELL ALWAYS"),
            "{bar:?}"
        );

        state.prefix_armed = Some(at);
        let bar = status_row(&render(&state, &app));
        assert!(
            bar.contains("prefix:") && bar.contains("BELL ALWAYS"),
            "{bar:?}"
        );
        state.prefix_armed = None;

        let bar = press_esc(&mut state, &mut app, at);
        assert!(
            bar.contains("Esc again") && bar.contains("BELL ALWAYS"),
            "{bar:?}"
        );
        let bar = press_esc(&mut state, &mut app, at + Duration::from_secs(1));
        assert!(
            bar.contains("Closing") && bar.contains("BELL ALWAYS"),
            "{bar:?}"
        );
    }

    /// The bar's row at an arbitrary terminal width.
    fn status_row_at_width(state: &EmporiumState, app: &App, width: u16) -> String {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, 12)).unwrap();
        let mut cache = HashMap::new();
        terminal
            .draw(|frame| {
                let _ = draw(
                    frame,
                    app,
                    state,
                    SystemTime::now(),
                    Instant::now(),
                    &mut cache,
                );
            })
            .unwrap();
        status_row(terminal.backend().buffer())
    }

    #[test]
    fn at_a_narrow_width_the_hints_give_way_before_the_badge_and_the_counts() {
        let (mut state, mut app, at) = quitting_fixture(0);
        engine::update(
            &mut state,
            &mut app,
            &BrigadeConfig::default(),
            bell_switch(true),
            at,
        );
        // Wide enough for the badge and the counts and a little of the hints.
        let bar = status_row_at_width(&state, &app, 24);
        assert!(bar.trim_end().ends_with("BELL ALWAYS [0/0]"), "{bar:?}");
        // Too narrow for both: the segment is clipped from its right end, the
        // badge survives whole and the hints are gone.
        let bar = status_row_at_width(&state, &app, 14);
        assert_eq!(bar.trim_end(), "BELL ALWAYS [0", "{bar:?}");
        assert!(!bar.contains("move"), "{bar:?}");
    }

    #[test]
    fn the_switch_survives_a_restart_through_the_store() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let (mut state, mut app, at) = quitting_fixture(0);
        // Nothing stored: off.
        assert_eq!(store.borrow().bell_when_focused().unwrap(), None);

        // The operator turns it on with F4; the core asks for the write and the
        // executor performs it.
        let f4 = key_event(
            banto_core::input::KeyCode::F(4),
            banto_core::input::Modifiers::NONE,
        );
        let cmds = engine::update(&mut state, &mut app, &BrigadeConfig::default(), f4, at);
        let mut written = 0;
        for cmd in cmds {
            if let Cmd::Store(intent) = cmd {
                written += 1;
                execute_store_intent(intent, &store);
            }
        }
        assert_eq!(written, 1);
        assert_eq!(store.borrow().bell_when_focused().unwrap(), Some(true));

        // A new run reads it back the way the loop does.
        let mut restarted = EmporiumState::new(PrefixKey::default());
        let on = store
            .borrow()
            .bell_when_focused()
            .ok()
            .flatten()
            .unwrap_or(false);
        engine::update(
            &mut restarted,
            &mut app,
            &BrigadeConfig::default(),
            bell_switch(on),
            at,
        );
        assert!(restarted.bell_when_focused());
    }

    #[test]
    fn the_agent_the_worker_picker_was_confirmed_with_survives_a_restart_through_the_store() {
        use banto_core::input::{KeyCode, Modifiers};
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let brigade = BrigadeConfig {
            worker_agent: banto_core::config::WorkerAgentSetting::Select,
            ..BrigadeConfig::default()
        };
        let mut app = App::new(vec![banto_core::model::SessionRow {
            id: "dir".to_string(),
            agent: AgentKind::ClaudeCode,
            title: Some("dir".to_string()),
            cwd: Some(PathBuf::from("/work")),
            dialog_open: false,
            activity: banto_core::model::Activity::Alive,
            is_agent: false,
            preview: None,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
            archived: false,
        }]);
        let at = Instant::now();
        // Press `B` and let the store answer the membership question, as the
        // loop does.
        let open_picker = |state: &mut EmporiumState, app: &mut App| {
            let mut cmds = engine::update(
                state,
                app,
                &brigade,
                key_event(KeyCode::Char('B'), Modifiers::SHIFT),
                at,
            );
            while let Some(cmd) = cmds.pop() {
                if let Cmd::Store(intent) = cmd {
                    for event in execute_store_intent(intent, &store) {
                        cmds.extend(engine::update(state, app, &brigade, event, at));
                    }
                }
            }
        };
        let picked = |app: &App| match app.modal() {
            Some(banto_core::app::Modal::WorkerAgentPicker(picker)) => picker.agent(),
            other => panic!("expected the picker, got {}", other.is_some()),
        };

        // Nothing stored: the picker opens on the Director's own product.
        assert_eq!(store.borrow().last_worker_agent().unwrap(), None);
        let mut state = EmporiumState::new(PrefixKey::default());
        state.focus = Focus::Sidebar;
        open_picker(&mut state, &mut app);
        assert_eq!(picked(&app), AgentKind::ClaudeCode);

        // Toggle to Codex and confirm: the core asks for the write once, and the
        // executor performs it.
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            key_event(KeyCode::BackTab, Modifiers::SHIFT),
            at,
        );
        let cmds = engine::update(
            &mut state,
            &mut app,
            &brigade,
            key_event(KeyCode::Enter, Modifiers::NONE),
            at,
        );
        let saves: Vec<Cmd> = cmds
            .into_iter()
            .filter(|cmd| matches!(cmd, Cmd::Store(StoreIntent::SaveLastWorkerAgent { .. })))
            .collect();
        assert_eq!(saves.len(), 1, "{saves:?}");
        for cmd in saves {
            if let Cmd::Store(intent) = cmd {
                execute_store_intent(intent, &store);
            }
        }
        assert_eq!(
            store.borrow().last_worker_agent().unwrap(),
            Some(AgentKind::Codex)
        );

        // A new run reads it back the way the loop does, and the picker opens
        // on it.
        let mut restarted = EmporiumState::new(PrefixKey::default());
        restarted.focus = Focus::Sidebar;
        engine::update(
            &mut restarted,
            &mut app,
            &brigade,
            Event::LastWorkerAgentLoaded {
                agent: store.borrow().last_worker_agent().ok().flatten(),
            },
            at,
        );
        open_picker(&mut restarted, &mut app);
        assert_eq!(picked(&app), AgentKind::Codex);
    }

    // --- sampling whether each hosted pane is idle -------------------------------

    /// The tick's samples as a map, from a gather over `state`.
    fn quiescence_of(
        state: &EmporiumState,
        store: &RefCell<Store>,
        claude_home: &Path,
        codex_home: Option<&Path>,
        app: &App,
    ) -> HashMap<String, Quiescence> {
        let mut gate = GlanceGate::default();
        let codex_home = codex_home.map(|path| CodexHome::new(path.to_path_buf()));
        gather_relay_observations(
            state,
            store,
            &ClaudeHome::new(claude_home.to_path_buf()),
            codex_home.as_ref(),
            app,
            None,
            &mut gate,
        );
        gate.quiescence
            .into_iter()
            .map(|(key, quiescence)| (key.as_str().to_string(), quiescence))
            .collect()
    }

    #[test]
    fn a_claude_panes_sample_follows_its_live_status() {
        for (status, expected) in [
            ("idle", Quiescence::Idle),
            ("busy", Quiescence::NotIdle),
            ("waiting", Quiescence::NotIdle),
            // Not idle for want of a turn, but the turn has ended: the one
            // status the quit question and the title tell from busy.
            ("shell", Quiescence::Background),
            ("something-new", Quiescence::NotIdle),
        ] {
            let claude_home = tempfile::tempdir().unwrap();
            let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
            write_live_state_with_status(
                claude_home.path(),
                std::process::id(),
                "w1",
                Path::new("/work"),
                status,
            );
            let samples = quiescence_of(&state, &store, claude_home.path(), None, &app);
            assert_eq!(samples["w1"], expected, "status {status:?}");
            // The Director has no live file: nothing read, so unknown.
            assert_eq!(samples["dir"], Quiescence::Unknown, "status {status:?}");
        }
    }

    #[test]
    fn a_pane_with_no_usable_live_file_is_unknown_never_idle() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        // A file for a process that is gone, and none at all for the Director.
        write_live_state(claude_home.path(), 4_000_000_000, "w1", Path::new("/work"));
        let samples = quiescence_of(&state, &store, claude_home.path(), None, &app);
        assert_eq!(samples["w1"], Quiescence::Unknown);
        assert_eq!(samples["dir"], Quiescence::Unknown);
    }

    #[test]
    fn a_member_in_the_shell_state_is_still_nudgeable_and_yet_in_the_background() {
        // The relay's own notion of idle enough to nudge is untouched: `shell`
        // is neither busy nor waiting. The quiescence answer is a separate one,
        // and says why it is not idle: a background command is running.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        write_live_state_with_status(
            claude_home.path(),
            std::process::id(),
            "w1",
            Path::new("/work"),
            "shell",
        );
        let mut gate = GlanceGate::default();
        let relay = gather_relay_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            None,
            &app,
            None,
            &mut gate,
        );
        let worker = relay.iter().find(|o| o.token == "worker-1").unwrap();
        assert_eq!(worker.is_idle_this_tick, Some(true));
        let samples: HashMap<_, _> = gate.quiescence.into_iter().collect();
        assert_eq!(samples[&SessionKey::from_id("w1")], Quiescence::Background);
    }

    #[test]
    fn a_session_in_the_shell_state_is_listed_busy_while_the_relay_still_nudges_it() {
        // One live file, three readers that must not be confused: the list
        // follows the work (a background command is running, so a green dot),
        // the tile title says the turn has ended (`idle`, with a mark), and the
        // relay follows the turn too (it can take a message).
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, mut app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        write_live_state_with_status(
            claude_home.path(),
            std::process::id(),
            "w1",
            Path::new("/work"),
            "shell",
        );
        let home = ClaudeHome::new(claude_home.path().to_path_buf());

        // The list's classification of the same session.
        let meta = banto_core::model::SessionMeta {
            id: banto_core::model::SessionId("w1".to_string()),
            agent: AgentKind::ClaudeCode,
            title: None,
            title_is_custom: false,
            cwd: None,
            source_path: PathBuf::from("w1.jsonl"),
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            is_agent: false,
            preview: None,
            source_archived: false,
        };
        let rows = crate::session::rows_from_metas(
            vec![meta],
            &home,
            None,
            &banto_core::status::AgeThresholds::default(),
        );
        assert_eq!(rows[0].activity, Activity::Busy, "shown as busy");
        assert!(!rows[0].dialog_open);
        app.replace_rows(rows);
        assert!(
            !app.is_waiting_id("w1"),
            "busy is not waiting: no mark, no count"
        );

        // The relay and the quit question, on the same file, as before.
        let mut gate = GlanceGate::default();
        let relay = gather_relay_observations(&state, &store, &home, None, &app, None, &mut gate);
        let worker = relay.iter().find(|o| o.token == "worker-1").unwrap();
        assert_eq!(worker.is_idle_this_tick, Some(true), "still nudgeable");
        let samples: HashMap<_, _> = gate.quiescence.into_iter().collect();
        assert_eq!(samples[&SessionKey::from_id("w1")], Quiescence::Background);
    }

    #[test]
    fn a_hosted_codex_pane_samples_from_its_last_turn_marker_in_or_out_of_a_brigade() {
        let claude_home = tempfile::tempdir().unwrap();
        let codex_home = tempfile::tempdir().unwrap();
        let (store, mut state, mut app) = staged_worker("w1", Some(AgentKind::Codex));
        write_codex_rollout(codex_home.path(), "w1", &["task_started"]);
        // A hosted Codex session that belongs to no brigade.
        state
            .screens
            .insert(SessionKey::from_id("solo"), Screen::new(24, 80));
        write_codex_rollout(
            codex_home.path(),
            "solo",
            &["task_started", "task_complete"],
        );
        app.replace_rows(vec![
            test_row("dir", AgentKind::ClaudeCode),
            test_row("w1", AgentKind::Codex),
            test_row("solo", AgentKind::Codex),
        ]);

        let samples = quiescence_of(
            &state,
            &store,
            claude_home.path(),
            Some(codex_home.path()),
            &app,
        );
        assert_eq!(samples["w1"], Quiescence::NotIdle, "task_started");
        assert_eq!(samples["solo"], Quiescence::Idle, "task_complete");

        // No marker, and no Codex home at all: unknown, never idle.
        write_codex_rollout(codex_home.path(), "none", &[]);
        state
            .screens
            .insert(SessionKey::from_id("none"), Screen::new(24, 80));
        app.replace_rows(vec![
            test_row("dir", AgentKind::ClaudeCode),
            test_row("w1", AgentKind::Codex),
            test_row("solo", AgentKind::Codex),
            test_row("none", AgentKind::Codex),
        ]);
        let samples = quiescence_of(
            &state,
            &store,
            claude_home.path(),
            Some(codex_home.path()),
            &app,
        );
        assert_eq!(samples["none"], Quiescence::Unknown);
        let samples = quiescence_of(&state, &store, claude_home.path(), None, &app);
        assert_eq!(samples["solo"], Quiescence::Unknown);
        assert_eq!(samples["w1"], Quiescence::Unknown);
    }

    #[test]
    fn a_session_opened_on_its_own_and_a_placeholder_key_are_sampled_too() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, mut state, mut app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        app.replace_rows(vec![
            test_row("dir", AgentKind::ClaudeCode),
            test_row("w1", AgentKind::ClaudeCode),
            test_row("solo", AgentKind::ClaudeCode),
        ]);
        state
            .screens
            .insert(SessionKey::from_id("solo"), Screen::new(24, 80));
        // A pane still awaiting its session id has nothing to look up.
        let placeholder = SessionKey::from_id("new::/work::0");
        assert!(placeholder.is_synthetic());
        state
            .screens
            .insert(placeholder.clone(), Screen::new(24, 80));
        write_live_state_with_status(
            claude_home.path(),
            std::process::id(),
            "solo",
            Path::new("/work"),
            "idle",
        );

        let samples = quiescence_of(&state, &store, claude_home.path(), None, &app);
        assert_eq!(samples["solo"], Quiescence::Idle);
        assert_eq!(samples[placeholder.as_str()], Quiescence::Unknown);
    }

    #[test]
    fn the_empty_pane_placeholder_offers_enter_brigade_and_quit_but_not_f2() {
        // Shown only while nothing is staged, when the list has the focus: F2 has
        // nowhere to go from there, so it is not offered.
        let state = EmporiumState::new(PrefixKey::default());
        let app = App::new(vec![test_row("dir", AgentKind::ClaudeCode)]);
        let buffer = render(&state, &app);
        let screen: String = (0..buffer.area.height)
            .map(|y| text_at(&buffer, 0, y, buffer.area.width))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("Select a session and press Enter."),
            "{screen}"
        );
        assert!(
            screen.contains("B brigade/disband · q/Esc quit."),
            "{screen}"
        );
        assert!(!screen.contains("F2 toggles"), "{screen}");
    }

    #[test]
    fn with_a_pane_focused_the_bar_names_the_prefix_and_none_of_the_list_keys() {
        let (mut state, app, _brigade) = staged_brigade_of_two_workers();
        state.status = None;
        state.focus = Focus::Pane;
        let bar = status_row(&render(&state, &app));
        assert!(bar.contains("Ctrl+B prefix"), "{bar:?}");
        assert!(bar.contains("F2 list"), "{bar:?}");
        assert!(
            bar.contains("F3 next pane"),
            "a brigade of several tiles: {bar:?}"
        );
        for list_only in [
            "j/k",
            "Enter open",
            "b +worker",
            "/ search",
            "Tab",
            "q quit",
        ] {
            assert!(!bar.contains(list_only), "{list_only:?} in {bar:?}");
        }

        // The list focused again: the list keys, worded for what they do.
        state.focus = Focus::Sidebar;
        let bar = status_row(&render(&state, &app));
        assert!(bar.contains("F2 pane"), "{bar:?}");
        assert!(bar.contains("j/k move"), "{bar:?}");
        assert!(!bar.contains("prefix"), "{bar:?}");
    }

    #[test]
    fn a_configured_prefix_is_shown_as_configured_and_a_single_session_has_no_f3() {
        let mut state = EmporiumState::new(PrefixKey::parse("C-a"));
        let app = App::new(vec![test_row("solo", AgentKind::ClaudeCode)]);
        let key = SessionKey::from_id("solo");
        state
            .screens
            .insert(key.clone(), banto_core::screen::Screen::new(24, 80));
        state.stage = Stage::Solo(key);
        state.focus = Focus::Pane;
        let bar = status_row(&render(&state, &app));
        assert!(bar.contains("Ctrl+A prefix · F2 list"), "{bar:?}");
        assert!(!bar.contains("F3"), "{bar:?}");
    }

    #[test]
    fn a_tile_being_moved_is_drawn_in_the_gesture_colour_with_its_label_ghosted_at_the_pointer() {
        use banto_core::input::{MouseButton, MouseEventKind};
        let (mut state, mut app, brigade) = staged_brigade_of_two_workers();
        let area = Rect::new(0, 0, 120, 40);
        let tiles = state.geometry(area).tiles;
        let (moved, other) = (tiles[1].1, tiles[2].1);
        // The Worker tiles' own colours before anything happens.
        let before = render(&state, &app);
        assert_ne!(before[(moved.x, moved.y + 3)].fg, DRAG_LINE_COLOR);

        // A bare press: the tile is marked, nothing follows the pointer.
        mouse(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            moved.x + 2,
            moved.y,
        );
        let pressed = render(&state, &app);
        for (x, y) in [
            (moved.x, moved.y + 3),
            (moved.x + moved.width - 1, moved.y + 3),
            (moved.x + 5, moved.y + moved.height - 1),
        ] {
            assert_eq!(pressed[(x, y)].fg, DRAG_LINE_COLOR, "border cell ({x},{y})");
        }
        assert_eq!(
            text_at(&pressed, moved.x + 1, moved.y, 10),
            " worker-1 ",
            "the text is unchanged"
        );
        // The moved tile's label is the gesture's own reversed yellow, not
        // the grey tab every other tile's label wears.
        let moved_label = &pressed[(moved.x + 2, moved.y)];
        assert_eq!(moved_label.fg, DRAG_LINE_COLOR);
        assert!(moved_label.modifier.contains(Modifier::REVERSED));
        assert_ne!(moved_label.bg, Color::Gray);
        assert_eq!(
            pressed[(other.x + 2, other.y)].bg,
            Color::Gray,
            "the tile that is not moved keeps its grey tab"
        );
        assert_ne!(
            pressed[(other.x, other.y + 3)].fg,
            DRAG_LINE_COLOR,
            "only the moved tile"
        );
        assert!(
            !text_at(&pressed, 50, 20, 10).contains("worker-1"),
            "no ghost before the pointer moves"
        );

        // Dragged over the master pane's interior, lower than the second
        // tile's top: the ghost is there, and a landing line under it.
        mouse(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Drag(MouseButton::Left),
            50,
            25,
        );
        let dragging = render(&state, &app);
        assert_eq!(text_at(&dragging, 50, 25, 10), " worker-1 ");
        for x in 50..60 {
            let cell = &dragging[(x, 25)];
            assert_eq!(cell.fg, DRAG_LINE_COLOR);
            assert!(
                cell.modifier.contains(Modifier::REVERSED),
                "reversed at {x}"
            );
        }
        assert_eq!(
            dragging[(60, 25)].fg,
            Color::Reset,
            "the ghost is exactly its text wide"
        );
        assert_eq!(
            dragging[(other.x + 4, other.y + other.height - 1)].symbol(),
            "\u{2501}"
        );
        assert_eq!(
            dragging[(moved.x, moved.y + 3)].fg,
            DRAG_LINE_COLOR,
            "still marked"
        );

        // A ghost near the right edge is cut to the screen.
        mouse(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Drag(MouseButton::Left),
            116,
            25,
        );
        let edge = render(&state, &app);
        assert_eq!(text_at(&edge, 116, 25, 4), " wor");
        assert_eq!(edge[(119, 25)].fg, DRAG_LINE_COLOR);

        // The release removes every cue.
        mouse(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            116,
            25,
        );
        let done = render(&state, &app);
        // Rows above the status bar, which has colours of its own.
        for y in 0..39 {
            for x in 36..120 {
                if done[(x, y)].fg == DRAG_LINE_COLOR {
                    panic!("a gesture-coloured cell is left at ({x},{y})");
                }
            }
        }
    }

    // --- the status word on an expanded tile's title row -----------------------------------

    fn render_at(state: &EmporiumState, app: &App, width: u16, tick: Instant) -> Buffer {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, 40)).unwrap();
        let mut cache = HashMap::new();
        terminal
            .draw(|frame| {
                let _ = draw(frame, app, state, SystemTime::now(), tick, &mut cache);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    const DIR: &str = "dir";
    const W1: &str = "new-worker::1::worker-1";
    const W2: &str = "new-worker::1::worker-2";

    /// A staged brigade of a Claude Director, a Claude Worker and a Codex Worker
    /// whose rows say `activities` (in that order), every pane having written
    /// something at the returned instant, drawn `width` columns wide.
    fn scene(
        width: u16,
        activities: [Activity; 3],
    ) -> (EmporiumState, App, BrigadeConfig, Instant) {
        let (mut state, mut app, brigade) = staged_brigade_of_two_workers_with_rows(vec![
            test_row(W1, AgentKind::ClaudeCode),
            test_row(W2, AgentKind::Codex),
        ]);
        let rows = [
            (DIR, AgentKind::ClaudeCode),
            (W1, AgentKind::ClaudeCode),
            (W2, AgentKind::Codex),
        ]
        .into_iter()
        .zip(activities)
        .map(|((id, agent), activity)| SessionRow {
            activity,
            ..test_row(id, agent)
        })
        .collect();
        app.replace_rows(rows);
        let now = Instant::now();
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized { width, height: 40 },
            now,
        );
        for id in [DIR, W1, W2] {
            said(&mut state, &mut app, &brigade, id, b"x", now);
        }
        // What the shell's tick would have read for each pane: a live file that
        // says busy or waiting is not idle, and one that says idle is. (A status
        // this build does not know is not idle either; the rows here do not
        // carry one.) A row with no live process has nothing to read.
        let samples = [DIR, W1, W2]
            .into_iter()
            .zip(activities)
            .filter_map(|(id, activity)| {
                let reading = match activity {
                    Activity::Busy | Activity::Waiting => Quiescence::NotIdle,
                    Activity::Alive => Quiescence::Idle,
                    Activity::Idle(_) => return None,
                };
                Some((SessionKey::from_id(id), reading))
            })
            .collect();
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::QuiescenceSampled { samples },
            now,
        );
        (state, app, brigade, now)
    }

    /// The tick's samples for the panes named, replacing whatever was read before.
    fn sampled(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        readings: &[(&str, Quiescence)],
        now: Instant,
    ) {
        let samples = readings
            .iter()
            .map(|(id, reading)| (SessionKey::from_id(id), *reading))
            .collect();
        engine::update(
            state,
            app,
            brigade,
            Event::QuiescenceSampled { samples },
            now,
        );
    }

    /// The title row of pane `id` as drawn now.
    fn title_of(state: &EmporiumState, app: &App, id: &str, now: Instant) -> String {
        let buffer = render_at(state, app, 160, now);
        title_row(state, &buffer, id, 160).0
    }

    fn said(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        id: &str,
        bytes: &[u8],
        now: Instant,
    ) {
        engine::update(
            state,
            app,
            brigade,
            Event::PtyOutput {
                key: SessionKey::from_id(id),
                chunk: bytes.to_vec(),
            },
            now,
        );
    }

    /// The text of the top border row of the tile of pane `id`, and the tile.
    fn title_row(state: &EmporiumState, buffer: &Buffer, id: &str, width: u16) -> (String, Rect) {
        let key = SessionKey::from_id(id);
        let rect = state
            .geometry(Rect::new(0, 0, width, 40))
            .tiles
            .iter()
            .find(|(tile, _)| *tile == key)
            .map(|(_, rect)| *rect)
            .unwrap();
        (text_at(buffer, rect.x, rect.y, rect.width), rect)
    }

    const REPORT_WORKING: &[u8] = b"\x1b]7501;state=working:app=codex\x1b\\";
    const REPORT_BLOCKED: &[u8] = b"\x1b]7501;state=blocked:app=codex:kind=permission\x1b\\";
    const REPORT_DONE: &[u8] = b"\x1b]7501;state=done:app=codex\x1b\\";
    const REPORT_IDLE: &[u8] = b"\x1b]7501;state=idle:app=codex\x1b\\";

    #[test]
    fn a_directors_tile_title_keeps_its_own_state_word_while_its_row_carries_a_brigade_mark() {
        // The mark is the row's dot and the tile's colour, never the session's
        // state: an idle Director marked for a quiet brigade still draws "idle",
        // and a busy one "busy", never "waiting".
        for (activity, word) in [(Activity::Busy, "busy"), (Activity::Alive, "idle")] {
            let (state, mut app, _, now) = scene(160, [activity, Activity::Alive, Activity::Alive]);
            let unmarked = {
                let buffer = render_at(&state, &app, 160, now);
                title_row(&state, &buffer, DIR, 160).0
            };
            app.set_brigade_marks([DIR.to_string()].into_iter().collect());
            assert!(app.has_brigade_mark(DIR), "the row is marked");
            let buffer = render_at(&state, &app, 160, now);
            let (marked, _) = title_row(&state, &buffer, DIR, 160);
            assert!(
                marked.contains(&format!(" Claude \u{b7} {word} ")),
                "{word}: the marked Director's title row is {marked:?}"
            );
            assert!(
                !marked.contains("waiting"),
                "{word}: a mark is not a waiting word: {marked:?}"
            );
            assert_eq!(marked, unmarked, "{word}: the title row does not change");
        }
    }

    #[test]
    fn a_hosted_codex_pane_at_its_prompt_reads_idle_before_and_after_an_unrelated_reload() {
        use banto_core::model::AgeBucket::Today;
        // The incident: after a restart the open Codex Worker read "idle" (its row
        // proving no live process) for a minute and then "alive" when an unrelated
        // list reload found its pid. It is hosted, and says itself it is idle.
        let (mut state, mut app, brigade, now) = scene(
            160,
            [Activity::Alive, Activity::Alive, Activity::Idle(Today)],
        );
        said(&mut state, &mut app, &brigade, W2, REPORT_IDLE, now);
        assert!(title_of(&state, &app, W2, now).contains(" Codex \u{b7} idle "));
        app.replace_rows(vec![
            SessionRow {
                activity: Activity::Alive,
                ..test_row(DIR, AgentKind::ClaudeCode)
            },
            SessionRow {
                activity: Activity::Alive,
                ..test_row(W1, AgentKind::ClaudeCode)
            },
            SessionRow {
                activity: Activity::Alive,
                ..test_row(W2, AgentKind::Codex)
            },
        ]);
        let title = title_of(&state, &app, W2, now);
        assert!(title.contains(" Codex \u{b7} idle "), "{title:?}");
        assert!(!title.contains("alive"), "{title:?}");
    }

    #[test]
    fn a_hosted_panes_word_is_what_banto_has_read_of_it_and_nothing_before_that() {
        use banto_core::model::AgeBucket::Today;
        // A Codex pane that never reports: the sample decides. A Claude pane whose
        // live file says something this build does not know reads as busy, as the
        // quit question counts it. Nothing read yet: no word at all.
        let (mut state, mut app, brigade, now) =
            scene(160, [Activity::Alive, Activity::Alive, Activity::Alive]);
        sampled(
            &mut state,
            &mut app,
            &brigade,
            &[
                (W2, Quiescence::NotIdle),
                (W1, Quiescence::NotIdle),
                (DIR, Quiescence::Idle),
            ],
            now,
        );
        let codex = title_of(&state, &app, W2, now);
        assert!(codex.contains(" Codex \u{b7} busy "), "{codex:?}");
        let claude = title_of(&state, &app, W1, now);
        assert!(claude.contains(" Claude \u{b7} busy "), "{claude:?}");
        let director = title_of(&state, &app, DIR, now);
        assert!(director.contains(" Claude \u{b7} idle "), "{director:?}");

        sampled(&mut state, &mut app, &brigade, &[], now);
        for id in [DIR, W1, W2] {
            let title = title_of(&state, &app, id, now);
            for word in ["idle", "busy", "alive", "waiting"] {
                assert!(!title.contains(word), "{id}: nothing read yet: {title:?}");
            }
        }
        // A row that says it has no live process changes nothing about that.
        app.replace_rows(vec![SessionRow {
            activity: Activity::Idle(Today),
            ..test_row(W2, AgentKind::Codex)
        }]);
        assert!(!title_of(&state, &app, W2, now).contains("idle"));
    }

    #[test]
    fn a_claude_pane_in_the_shell_state_says_idle_with_a_background_mark_on_its_title() {
        // The turn has ended and a background command runs on: the title says
        // idle and why that is not the whole story. Busy is still busy, and a
        // row that says waiting outranks the sample, mark and all.
        let (mut state, mut app, brigade, now) =
            scene(160, [Activity::Alive, Activity::Alive, Activity::Alive]);
        sampled(
            &mut state,
            &mut app,
            &brigade,
            &[(W1, Quiescence::Background), (DIR, Quiescence::NotIdle)],
            now,
        );
        let title = title_of(&state, &app, W1, now);
        assert!(
            title.contains(" Claude \u{b7} idle \u{b7} bg "),
            "{title:?}"
        );
        let director = title_of(&state, &app, DIR, now);
        assert!(director.contains(" Claude \u{b7} busy "), "{director:?}");
        assert!(!director.contains("bg"), "{director:?}");

        sampled(
            &mut state,
            &mut app,
            &brigade,
            &[(W1, Quiescence::Idle)],
            now,
        );
        let title = title_of(&state, &app, W1, now);
        assert!(title.contains(" Claude \u{b7} idle "), "{title:?}");
        assert!(!title.contains("bg"), "{title:?}");

        // The same sample under a row that says waiting: waiting, no mark.
        let (mut state, mut app, brigade, now) =
            scene(160, [Activity::Alive, Activity::Waiting, Activity::Alive]);
        sampled(
            &mut state,
            &mut app,
            &brigade,
            &[(W1, Quiescence::Background)],
            now,
        );
        let title = title_of(&state, &app, W1, now);
        assert!(title.contains(" Claude \u{b7} waiting "), "{title:?}");
        assert!(!title.contains("bg"), "{title:?}");
    }

    #[test]
    fn a_collapsed_and_an_expanded_tile_render_the_same_title_row_for_every_state_and_product() {
        use banto_core::model::AgeBucket::Today;
        // What it is, the row's activity, what the Codex pane reported, and the
        // status the title and the strip both show.
        type Case = (
            &'static str,
            AgentKind,
            Activity,
            Option<&'static [u8]>,
            &'static str,
        );
        let cases: [Case; 12] = [
            (
                "codex working",
                AgentKind::Codex,
                Activity::Alive,
                Some(REPORT_WORKING),
                "Codex \u{b7} busy",
            ),
            (
                "codex blocked",
                AgentKind::Codex,
                Activity::Alive,
                Some(REPORT_BLOCKED),
                "Codex \u{b7} waiting",
            ),
            (
                "codex done",
                AgentKind::Codex,
                Activity::Alive,
                Some(REPORT_DONE),
                "Codex \u{b7} idle",
            ),
            (
                "codex idle",
                AgentKind::Codex,
                Activity::Alive,
                Some(REPORT_IDLE),
                "Codex \u{b7} idle",
            ),
            (
                "codex idle, its row proving no live process",
                AgentKind::Codex,
                Activity::Idle(Today),
                Some(REPORT_IDLE),
                "Codex \u{b7} idle",
            ),
            (
                "codex no report, read idle",
                AgentKind::Codex,
                Activity::Alive,
                None,
                "Codex \u{b7} idle",
            ),
            (
                "claude busy",
                AgentKind::ClaudeCode,
                Activity::Busy,
                None,
                "Claude \u{b7} busy",
            ),
            (
                "claude waiting",
                AgentKind::ClaudeCode,
                Activity::Waiting,
                None,
                "Claude \u{b7} waiting",
            ),
            (
                "claude at its prompt",
                AgentKind::ClaudeCode,
                Activity::Alive,
                None,
                "Claude \u{b7} idle",
            ),
            (
                "claude, no live file: nothing to say",
                AgentKind::ClaudeCode,
                Activity::Idle(Today),
                None,
                "Claude",
            ),
            (
                "claude busy (a report is not read for it)",
                AgentKind::ClaudeCode,
                Activity::Busy,
                Some(REPORT_IDLE),
                "Claude \u{b7} busy",
            ),
            (
                "codex, nothing known: nothing to say",
                AgentKind::Codex,
                Activity::Idle(Today),
                None,
                "Codex",
            ),
        ];
        for (what, agent, activity, report, expected) in cases {
            // The pane under test is the Worker of that product.
            let id = if agent == AgentKind::Codex { W2 } else { W1 };
            let mut activities = [Activity::Alive; 3];
            activities[if agent == AgentKind::Codex { 2 } else { 1 }] = activity;
            let (mut state, mut app, brigade, now) = scene(160, activities);
            if let Some(bytes) = report {
                said(&mut state, &mut app, &brigade, id, bytes, now);
            }
            let buffer = render_at(&state, &app, 160, now);
            let (row, open_tile) = title_row(&state, &buffer, id, 160);
            assert!(
                row.contains(&format!(" {expected} ")),
                "{what}: the title row is {row:?}"
            );
            // Collapsed, the same tile has the same title row, cell for cell,
            // but for the button that now opens it.
            let key = SessionKey::from_id(id);
            toggle_minimize_for_test(&mut state, &mut app, &brigade, &key);
            let collapsed_buffer = render_at(&state, &app, 160, now);
            let (collapsed_row, strip) = title_row(&state, &collapsed_buffer, id, 160);
            assert_eq!(strip.height, banto_core::engine::COLLAPSED_TILE_HEIGHT);
            assert_eq!(
                collapsed_row,
                row.replace("[_]", "[+]"),
                "{what}: collapsed or open, one title row"
            );
            for x in strip.x..strip.x + strip.width {
                let (open, shut) = (&buffer[(x, open_tile.y)], &collapsed_buffer[(x, strip.y)]);
                if open.symbol() != "_" && open.symbol() != "+" {
                    assert_eq!(
                        (open.fg, open.bg, open.modifier),
                        (shut.fg, shut.bg, shut.modifier),
                        "{what}: column {x}"
                    );
                }
            }
        }
    }

    /// Collapse `key`'s tile the way the operator does, with its button.
    fn toggle_minimize_for_test(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        key: &SessionKey,
    ) {
        use banto_core::input::{MouseButton, MouseEventKind};
        let button = state
            .geometry(Rect::new(0, 0, 160, 40))
            .buttons()
            .iter()
            .find(|b| &b.key == key && b.kind == ButtonKind::Minimize)
            .cloned()
            .unwrap();
        mouse(
            state,
            app,
            brigade,
            MouseEventKind::Down(MouseButton::Left),
            button.rect.x,
            button.rect.y,
        );
    }

    /// The colour of the first cell of `word` on the title row of pane `id`.
    fn word_colour(
        state: &EmporiumState,
        buffer: &Buffer,
        id: &str,
        width: u16,
        word: &str,
    ) -> Color {
        let (row, rect) = title_row(state, buffer, id, width);
        let cells: Vec<char> = row.chars().collect();
        let needle: Vec<char> = format!("{word} ").chars().collect();
        let at = cells
            .windows(needle.len())
            .position(|window| window == needle.as_slice())
            .expect("the word is on the title row") as u16;
        buffer[(rect.x + at, rect.y)].fg
    }

    /// What a tile's title row says between its label and its buttons, trimmed.
    fn status_segment(state: &EmporiumState, buffer: &Buffer, id: &str, width: u16) -> String {
        let key = SessionKey::from_id(id);
        let geometry = state.geometry(Rect::new(0, 0, width, 40));
        let rect = geometry
            .tiles
            .iter()
            .find(|(tile, _)| *tile == key)
            .map(|(_, rect)| *rect)
            .unwrap();
        let (_, label_end) = banto_core::layout::title_columns(rect, &state.tile_label(&key));
        let buttons_at = geometry
            .buttons()
            .iter()
            .filter(|button| button.key == key)
            .map(|button| button.rect.x)
            .min()
            .unwrap_or(rect.right() - 1);
        text_at(
            buffer,
            label_end + 1,
            rect.y,
            buttons_at - 1 - (label_end + 1),
        )
        .trim_matches(|c: char| c == ' ' || c == '\u{2500}')
        .to_string()
    }

    #[test]
    fn the_title_status_loses_the_agent_as_the_tile_narrows_and_the_word_is_never_cut() {
        // A Claude Worker waiting and a Codex Worker working after its turn (the
        // rollout says idle; the report says working), in the stack column, whose
        // width is `(width - 36) / 2`. Both labels are 8 cells.
        let segments_at = |width: u16| {
            let (mut state, mut app, brigade, now) =
                scene(width, [Activity::Busy, Activity::Waiting, Activity::Alive]);
            said(&mut state, &mut app, &brigade, W2, REPORT_WORKING, now);
            let buffer = render_at(&state, &app, width, now);
            [W1, W2].map(|id| status_segment(&state, &buffer, id, width))
        };
        // Tiles 62 wide, and 42: everything.
        let all = ["Claude \u{b7} waiting", "Codex \u{b7} busy"];
        assert_eq!(segments_at(160), all);
        assert_eq!(segments_at(120), all);
        // 37 wide: `Claude · waiting` (16 cells) no longer fits beside an 8-cell
        // label, so only that tile loses its agent; `Codex · busy` (12) still does.
        assert_eq!(segments_at(110), ["waiting", "Codex \u{b7} busy"]);
        // 32 wide: every agent label has gone.
        assert_eq!(segments_at(100), ["waiting", "busy"]);
        // 22 wide: no room even for the word, so none of it: never half a word.
        assert_eq!(segments_at(80), ["", ""]);
    }

    /// A staged brigade of the Claude Director, a Claude Worker whose label is
    /// `worker-1` (8 cells), or `{name} (worker-1)` when `name` is given, and a
    /// Codex Worker, `width` columns wide, the Claude Worker alive. The stack
    /// column is `(width - 36) / 2` wide.
    fn labelled_pair(
        width: u16,
        name: Option<&str>,
    ) -> (EmporiumState, App, BrigadeConfig, Instant) {
        let (mut state, mut app, brigade, now) =
            scene(width, [Activity::Alive, Activity::Alive, Activity::Alive]);
        if let Some(name) = name {
            name_worker_1(&mut state, &mut app, &brigade, name, now);
        }
        (state, app, brigade, now)
    }

    /// Give `worker-1` the display name `name`, so its label is `name (worker-1)`.
    fn name_worker_1(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        name: &str,
        now: Instant,
    ) {
        engine::update(
            state,
            app,
            brigade,
            Event::Tick {
                relay: Vec::new(),
                brigades: vec![banto_core::engine::BrigadeNameFacts {
                    brigade_id: 1,
                    name: "cell".to_string(),
                    director_session: None,
                    member_names: vec![("worker-1".to_string(), name.to_string())],
                    request: None,
                    interrupts: None,
                }],
            },
            now,
        );
    }

    /// `実装`: 4 cells, so the label `実装 (worker-1)` is 15.
    const IMPLEMENTATION: &str = "\u{5b9f}\u{88c5}";
    /// `レビュー`: 8 cells, so the label `レビュー (worker-1)` is 19.
    const REVIEW: &str = "\u{30ec}\u{30d3}\u{30e5}\u{30fc}";

    #[test]
    fn the_title_status_steps_happen_at_these_tile_widths_for_an_8_and_a_15_cell_label() {
        // The status needs the tile width minus the label's cells and 14 for the
        // corners, the padding and the buttons: `idle` (4) at label + 18, and
        // `Claude · idle` (13) at label + 27.
        let steps = |label: u16| -> Vec<(u16, &'static str)> {
            vec![
                (label + 27, "Claude \u{b7} idle"),
                (label + 26, "idle"),
                (label + 18, "idle"),
                (label + 17, ""),
            ]
        };
        for (name, label) in [(None, 8u16), (Some(IMPLEMENTATION), 15)] {
            for (tile_width, expected) in steps(label) {
                let terminal = 2 * tile_width + 36;
                let (state, app, _, tick) = labelled_pair(terminal, name);
                let buffer = render_at(&state, &app, terminal, tick);
                let (row, rect) = title_row(&state, &buffer, W1, terminal);
                assert_eq!(
                    rect.width, tile_width,
                    "the stack column at terminal {terminal}"
                );
                assert_eq!(
                    status_segment(&state, &buffer, W1, terminal),
                    expected,
                    "label of {label} cells, tile {tile_width} wide: {row:?}"
                );
            }
        }
    }

    #[test]
    fn at_the_default_collapsed_width_a_long_label_leaves_no_room_for_a_status() {
        // All collapsed, terminal 120: the stack column is 36 wide, so the status
        // gets 36 - 14 - the label's cells: `Claude · idle` for `worker-1` (8), the
        // word alone for `実装 (worker-1)` (15), and nothing from 19 cells up, as
        // for `レビュー (worker-11)` (20).
        for (name, expected) in [
            (None, "Claude \u{b7} idle"),
            (Some(IMPLEMENTATION), "idle"),
            (Some(REVIEW), ""),
        ] {
            let (mut state, mut app, brigade, tick) = labelled_pair(120, name);
            for id in [W1, W2] {
                toggle_minimize_for_test_at(
                    &mut state,
                    &mut app,
                    &brigade,
                    &SessionKey::from_id(id),
                    120,
                );
            }
            let buffer = render_at(&state, &app, 120, tick);
            let (_, rect) = title_row(&state, &buffer, W1, 120);
            assert_eq!(
                rect.width,
                banto_core::layout::DEFAULT_COLLAPSED_STACK_WIDTH
            );
            assert_eq!(
                status_segment(&state, &buffer, W1, 120),
                expected,
                "named {name:?}"
            );
        }
    }

    /// The column after the last glyph of `id`'s status, the column after its
    /// label's last cell, and the column of its first button.
    fn status_columns(
        state: &EmporiumState,
        buffer: &Buffer,
        id: &str,
        width: u16,
    ) -> (u16, u16, u16) {
        let key = SessionKey::from_id(id);
        let geometry = state.geometry(Rect::new(0, 0, width, 40));
        let rect = geometry
            .tiles
            .iter()
            .find(|(tile, _)| *tile == key)
            .map(|(_, rect)| *rect)
            .unwrap();
        let buttons_at = geometry
            .buttons()
            .iter()
            .filter(|button| button.key == key)
            .map(|button| button.rect.x)
            .min()
            .unwrap_or(rect.right() - 1);
        let (_, label_end) = banto_core::layout::title_columns(rect, &state.tile_label(&key));
        let last_glyph = (label_end + 1..buttons_at)
            .rev()
            .find(|x| {
                let symbol = buffer[(*x, rect.y)].symbol();
                symbol != " " && symbol != "\u{2500}"
            })
            .expect("a status is drawn");
        (last_glyph + 1, label_end, buttons_at)
    }

    #[test]
    fn the_status_is_anchored_on_the_buttons_so_the_word_ends_in_one_column_on_every_stack_tile() {
        // The Claude Worker's row, what the Codex Worker reports, the Claude
        // Worker's display name, whether its turn has ended behind a background
        // command, and what the two tiles read.
        type Case = (
            Activity,
            &'static [u8],
            Option<&'static str>,
            bool,
            [&'static str; 2],
        );
        let cases: [Case; 3] = [
            // A waiting tile beside an idle one.
            (
                Activity::Waiting,
                REPORT_IDLE,
                Some(REVIEW),
                false,
                ["Claude \u{b7} waiting", "Codex \u{b7} idle"],
            ),
            // An idle tile with a `bg` mark beside a waiting one.
            (
                Activity::Alive,
                REPORT_BLOCKED,
                Some(REVIEW),
                true,
                ["Claude \u{b7} idle \u{b7} bg", "Codex \u{b7} waiting"],
            ),
            // The same with both labels plain: 8 cells each, so the status has
            // the same room either way and only its own width differs.
            (
                Activity::Alive,
                REPORT_BLOCKED,
                None,
                true,
                ["Claude \u{b7} idle \u{b7} bg", "Codex \u{b7} waiting"],
            ),
        ];
        for (index, (claude, codex, name, background, expected)) in cases.into_iter().enumerate() {
            let (mut state, mut app, brigade, now) =
                scene(160, [Activity::Alive, claude, Activity::Alive]);
            if let Some(name) = name {
                name_worker_1(&mut state, &mut app, &brigade, name, now);
            }
            said(&mut state, &mut app, &brigade, W2, codex, now);
            if background {
                sampled(
                    &mut state,
                    &mut app,
                    &brigade,
                    &[(W1, Quiescence::Background)],
                    now,
                );
            }
            let buffer = render_at(&state, &app, 160, now);
            let segments = [W1, W2].map(|id| status_segment(&state, &buffer, id, 160));
            assert_eq!(segments, expected, "case {index}");
            let [
                (first_end, first_label, first_buttons),
                (second_end, second_label, second_buttons),
            ] = [W1, W2].map(|id| status_columns(&state, &buffer, id, 160));
            // Every stack tile has the stack's x and width and the same buttons ...
            assert_eq!(first_buttons, second_buttons, "case {index}");
            // ... so the word ends on one column, two cells short of the first
            // button: the pad, and the border cell that stays free.
            assert_eq!(
                first_end, second_end,
                "case {index}: the last column of the word"
            );
            assert_eq!(first_end + 2, first_buttons, "case {index}");
            // Fixture check: the first two cases are the ones where the labels
            // differ, which is what a status drawn from the label's end would
            // follow; the third has statuses of different widths instead.
            assert_eq!(name.is_some(), first_label != second_label, "case {index}");
        }
    }

    fn toggle_minimize_for_test_at(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        key: &SessionKey,
        width: u16,
    ) {
        use banto_core::input::{MouseButton, MouseEventKind};
        let button = state
            .geometry(Rect::new(0, 0, width, 40))
            .buttons()
            .iter()
            .find(|b| &b.key == key && b.kind == ButtonKind::Minimize)
            .cloned()
            .unwrap();
        mouse(
            state,
            app,
            brigade,
            MouseEventKind::Down(MouseButton::Left),
            button.rect.x,
            button.rect.y,
        );
    }

    /// A staged brigade of `workers` Workers on a `width` x `height` terminal,
    /// every pane hosted, drawn once.
    fn crowded_brigade(
        workers: usize,
        width: u16,
        height: u16,
    ) -> (EmporiumState, App, BrigadeConfig, Instant) {
        let (mut state, mut app, brigade) =
            staged_brigade_of_workers_with_rows(workers, Vec::new());
        let now = Instant::now();
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized { width, height },
            now,
        );
        (state, app, brigade, now)
    }

    fn render_sized(
        state: &EmporiumState,
        app: &App,
        width: u16,
        height: u16,
        tick: Instant,
    ) -> Buffer {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut cache = HashMap::new();
        terminal
            .draw(|frame| {
                let _ = draw(frame, app, state, SystemTime::now(), tick, &mut cache);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn a_tile_with_no_rows_draws_nothing_and_leaves_its_neighbours_buttons_clickable() {
        for (workers, width, height) in [
            (24usize, 120u16, 24u16),
            (40, 120, 24),
            (6, 120, 3),
            (3, 120, 2),
        ] {
            let (mut state, app, _brigade, now) = crowded_brigade(workers, width, height);
            let area = Rect::new(0, 0, width, height);
            let geometry = state.geometry(area);
            let empty: Vec<SessionKey> = geometry
                .tiles
                .iter()
                .filter(|(_, rect)| banto_core::layout::is_empty(*rect))
                .map(|(key, _)| key.clone())
                .collect();
            assert!(
                !empty.is_empty(),
                "{workers} workers on {height} rows: a tile with no rows"
            );

            // Nothing of an empty tile is drawn: the frame is the same cell for
            // cell as one where those panes are not hosted at all.
            let with = render_sized(&state, &app, width, height, now);
            for key in &empty {
                state.screens.remove(key);
            }
            let without = render_sized(&state, &app, width, height, now);
            assert_eq!(with, without, "{workers} workers on {height} rows");

            // Every button belongs to a tile that has rows, sits inside it, is
            // painted there, and is what a press on its cells finds.
            for button in geometry.buttons() {
                let tile = geometry
                    .tiles
                    .iter()
                    .find(|(k, _)| *k == button.key)
                    .unwrap()
                    .1;
                assert!(!banto_core::layout::is_empty(tile), "{button:?}");
                assert!(tile.contains(button.rect.as_position()));
                assert_eq!(
                    text_at(&with, button.rect.x, button.rect.y, button.rect.width),
                    button.text
                );
                for x in button.rect.x..button.rect.right() {
                    let hit = geometry.button_at(x, button.rect.y).unwrap();
                    assert_eq!((&hit.key, hit.kind), (&button.key, button.kind));
                }
            }
            // And the empty tiles own none, nor the move handle.
            for key in &empty {
                assert!(geometry.buttons().iter().all(|b| &b.key != key));
                for y in 0..height {
                    for x in 0..width {
                        assert_ne!(geometry.label_at(x, y), Some(key), "({x},{y})");
                    }
                }
            }
        }
    }

    #[test]
    fn a_solo_pane_at_degenerate_sizes_draws_without_panicking() {
        for width in 0..=3u16 {
            for height in 0..=3u16 {
                let mut state = EmporiumState::new(PrefixKey::default());
                let mut app = App::new(vec![test_row("solo", AgentKind::ClaudeCode)]);
                let key = SessionKey::from_id("solo");
                state.screens.insert(key.clone(), Screen::new(24, 80));
                state.stage = Stage::Solo(key);
                state.focus = Focus::Pane;
                let now = Instant::now();
                engine::update(
                    &mut state,
                    &mut app,
                    &BrigadeConfig::default(),
                    Event::Resized { width, height },
                    now,
                );
                let buffer = render_sized(&state, &app, width, height, now);
                assert_eq!((buffer.area.width, buffer.area.height), (width, height));
            }
        }
    }

    #[test]
    fn nothing_panics_or_is_drawn_outside_the_terminal_at_degenerate_sizes() {
        for workers in [0usize, 1, 2, 6] {
            for width in 0..=3u16 {
                for height in 0..=3u16 {
                    let (state, app, _brigade, now) = crowded_brigade(workers, width, height);
                    let buffer = render_sized(&state, &app, width, height, now);
                    assert_eq!(
                        (buffer.area.width, buffer.area.height),
                        (width, height),
                        "{workers} workers at {width}x{height}"
                    );
                    // Hit tests at every cell, empty or not.
                    let geometry = state.geometry(Rect::new(0, 0, width, height));
                    for y in 0..=height {
                        for x in 0..=width {
                            let _ = (
                                geometry.button_at(x, y),
                                geometry.label_at(x, y),
                                geometry.drop_slot(y),
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_degraded_stack_gives_a_collapsed_tile_three_two_or_one_row_and_never_loses_its_title_row()
    {
        // Workers collapsed on a 24-row terminal (23 stack rows): 7 get 3 or 4
        // rows each, 11 get 2 or 3, 23 get 1.
        for (workers, smallest) in [(5usize, 4u16), (7, 3), (11, 2), (23, 1)] {
            let (mut state, mut app, brigade) =
                staged_brigade_of_workers_with_rows(workers, Vec::new());
            let area = Rect::new(0, 0, 120, 24);
            let now = Instant::now();
            engine::update(
                &mut state,
                &mut app,
                &brigade,
                Event::Resized {
                    width: 120,
                    height: 24,
                },
                now,
            );
            for n in 1..=workers {
                let key = SessionKey::from_id(&format!("new-worker::1::worker-{n}"));
                let button = state
                    .geometry(area)
                    .buttons()
                    .iter()
                    .find(|b| b.key == key && b.kind == ButtonKind::Minimize)
                    .cloned();
                if let Some(button) = button {
                    mouse(
                        &mut state,
                        &mut app,
                        &brigade,
                        banto_core::input::MouseEventKind::Down(
                            banto_core::input::MouseButton::Left,
                        ),
                        button.rect.x,
                        button.rect.y,
                    );
                }
            }
            let geometry = state.geometry(area);
            assert!(geometry.all_collapsed, "{workers} workers");
            let heights: Vec<u16> = geometry.tiles[1..].iter().map(|(_, r)| r.height).collect();
            assert_eq!(
                *heights.iter().min().unwrap(),
                smallest,
                "{workers}: {heights:?}"
            );
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
            let mut cache = HashMap::new();
            terminal
                .draw(|frame| {
                    let _ = draw(frame, &app, &state, SystemTime::now(), now, &mut cache);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            for (key, rect) in &geometry.tiles[1..] {
                let title = text_at(buffer, rect.x, rect.y, rect.width);
                assert!(
                    title.contains(" worker-"),
                    "{workers} workers, {} rows: the title row of {} is {title:?}",
                    rect.height,
                    key.as_str()
                );
                if rect.height >= 2 {
                    assert!(title.contains("[S][+]"), "{title:?}");
                }
            }
        }
    }

    #[test]
    fn the_status_shows_no_output_age_however_long_ago_the_pane_wrote() {
        let (state, app, _, now) = scene(160, [Activity::Alive, Activity::Alive, Activity::Alive]);
        for quiet in [0, 25, 185, 7300] {
            let buffer = render_at(&state, &app, 160, now + Duration::from_secs(quiet));
            assert_eq!(
                status_segment(&state, &buffer, DIR, 160),
                "Claude \u{b7} idle",
                "{quiet}s after the pane wrote"
            );
            assert!(
                !title_row(&state, &buffer, DIR, 160).0.contains("ago"),
                "{quiet}s"
            );
        }
    }

    #[test]
    fn the_status_word_is_in_the_strips_colour_and_a_focused_title_keeps_only_its_label_reversed() {
        let (mut state, mut app, brigade, now) =
            scene(160, [Activity::Busy, Activity::Waiting, Activity::Alive]);
        said(&mut state, &mut app, &brigade, W2, REPORT_WORKING, now);
        let buffer = render_at(&state, &app, 160, now);
        let word_cell = |id: &str, word: &str| -> (Color, Modifier) {
            let (row, rect) = title_row(&state, &buffer, id, 160);
            let column = row
                .chars()
                .collect::<Vec<_>>()
                .windows(word.chars().count() + 1)
                .position(|w| w.iter().collect::<String>() == format!("{word} "))
                .expect("the word is on the title row") as u16;
            let cell = &buffer[(rect.x + column, rect.y)];
            (cell.fg, cell.modifier)
        };
        let colour = |activity| banto_tui::view::activity_color(activity);
        assert_eq!(word_cell(DIR, "busy").0, colour(Activity::Busy));
        assert_eq!(word_cell(W1, "waiting").0, WAITING_ACTIVITY_COLOR);
        assert_eq!(word_cell(W2, "busy").0, colour(Activity::Busy));
        // The Director's tile is the focused one: its label is reversed, its
        // status is not.
        assert_eq!(state.focus, Focus::Pane);
        let (_, dir) = title_row(&state, &buffer, DIR, 160);
        assert!(
            buffer[(dir.x + 2, dir.y)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(!word_cell(DIR, "busy").1.contains(Modifier::REVERSED));
        // The same words when the list has the focus instead.
        let focused_words = status_segment(&state, &buffer, DIR, 160);
        state.focus = Focus::Sidebar;
        let buffer = render_at(&state, &app, 160, now);
        assert_eq!(status_segment(&state, &buffer, DIR, 160), focused_words);
        assert!(
            !buffer[(dir.x + 2, dir.y)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(buffer[(dir.x + 2, dir.y)].bg, Color::Gray);
    }

    /// The cells of `id`'s label on its title row, the one-space padding on each
    /// side included, and the cells of the stretch between the label and the
    /// buttons, where the status is drawn.
    fn label_and_status_cells(
        state: &EmporiumState,
        buffer: &Buffer,
        id: &str,
        width: u16,
    ) -> (Vec<ratatui::buffer::Cell>, Vec<ratatui::buffer::Cell>) {
        let key = SessionKey::from_id(id);
        let geometry = state.geometry(Rect::new(0, 0, width, 40));
        let rect = geometry
            .tiles
            .iter()
            .find(|(tile, _)| *tile == key)
            .map(|(_, rect)| *rect)
            .unwrap();
        let (start, end) = banto_core::layout::title_columns(rect, &state.tile_label(&key));
        let buttons_at = geometry
            .buttons()
            .iter()
            .filter(|button| button.key == key)
            .map(|button| button.rect.x)
            .min()
            .unwrap_or(rect.right() - 1);
        let cells = |from: u16, to: u16| {
            (from..to)
                .map(|x| buffer[(x, rect.y)].clone())
                .collect::<Vec<_>>()
        };
        (cells(start, end), cells(end, buttons_at))
    }

    #[test]
    fn every_unfocused_tile_label_is_a_slate_tab_and_its_status_has_no_background() {
        // Focus on the list, so no tile is the focused one: the Director's tile,
        // an open stack tile and a collapsed strip all go through the same label
        // style. Worker 1 is Waiting, so its border is magenta; its label is
        // slate all the same.
        let (mut state, mut app, brigade, now) =
            scene(160, [Activity::Busy, Activity::Waiting, Activity::Alive]);
        state.focus = Focus::Sidebar;
        let collapsed = SessionKey::from_id(W2);
        toggle_minimize_for_test(&mut state, &mut app, &brigade, &collapsed);
        let buffer = render_at(&state, &app, 160, now);
        for (what, id) in [
            ("the Director's tile", DIR),
            ("an open stack tile", W1),
            ("a collapsed strip", W2),
        ] {
            let (label, status) = label_and_status_cells(&state, &buffer, id, 160);
            assert!(label.len() >= 3, "{what}: a label with its padding");
            for (at, cell) in label.iter().enumerate() {
                assert_eq!(
                    (cell.fg, cell.bg, cell.modifier),
                    (Color::Black, Color::Gray, Modifier::empty()),
                    "{what}: label cell {at}, {:?}",
                    cell.symbol()
                );
            }
            // The padding is part of the tab, one space on each side.
            assert_eq!(label.first().unwrap().symbol(), " ", "{what}");
            assert_eq!(label.last().unwrap().symbol(), " ", "{what}");
            assert!(
                status.iter().any(|cell| cell.symbol() != "\u{2500}"),
                "{what}: fixture check: there is a status on the row"
            );
            for (at, cell) in status.iter().enumerate() {
                assert_eq!(
                    cell.bg,
                    Color::Reset,
                    "{what}: status cell {at}, {:?}",
                    cell.symbol()
                );
            }
        }
    }

    #[test]
    fn the_focused_tile_label_stays_a_reversed_cyan_tab_and_its_status_has_no_background() {
        let (state, app, _brigade, now) =
            scene(160, [Activity::Busy, Activity::Waiting, Activity::Alive]);
        let buffer = render_at(&state, &app, 160, now);
        assert_eq!(state.focus, Focus::Pane);
        let (label, status) = label_and_status_cells(&state, &buffer, DIR, 160);
        for cell in &label {
            assert_eq!(cell.fg, Color::Cyan);
            assert!(cell.modifier.contains(Modifier::REVERSED));
        }
        assert!(status.iter().all(|cell| cell.bg == Color::Reset));
    }

    #[test]
    fn the_empty_stage_placeholder_keeps_the_plain_default_look() {
        // It labels an empty area, not a tile: default foreground, no
        // background, no modifier, whatever the tile tab looks like.
        let state = EmporiumState::new(PrefixKey::default());
        let app = App::new(vec![test_row("dir", AgentKind::ClaudeCode)]);
        let buffer = render(&state, &app);
        let (x, y) = (0..buffer.area.height)
            .find_map(|y| {
                let row = text_at(&buffer, 0, y, buffer.area.width);
                row.find(" session ")
                    .map(|at| (row[..at].chars().count() as u16, y))
            })
            .expect("the placeholder's label is drawn");
        for at in x..x + " session ".len() as u16 {
            let cell = &buffer[(at, y)];
            assert_eq!(
                (cell.fg, cell.bg, cell.modifier),
                (Color::Reset, Color::Reset, Modifier::empty()),
                "column {at}"
            );
        }
    }

    #[test]
    fn a_codex_pane_with_no_row_shows_what_it_reported_on_the_title_and_the_strip() {
        // The relay has resolved worker-1 as Codex (a member the session list has
        // not picked up), so its report is believed: busy, then waiting, on both
        // surfaces, as it would be with a row. worker-2 has neither a row nor a
        // known product, so what it prints is not read.
        let (mut state, mut app, brigade) = staged_brigade_of_two_workers();
        let key = SessionKey::from_id(W1);
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![RelayObservation {
                    brigade_id: 1,
                    token: "worker-1".to_string(),
                    key: key.clone(),
                    has_unseen: false,
                    is_idle_this_tick: None,
                    agent: Some(AgentKind::Codex),
                    live: None,
                    glance: None,
                    briefed_since_launch: None,
                }],
                brigades: Vec::new(),
            },
            Instant::now(),
        );
        let now = Instant::now();
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 160,
                height: 40,
            },
            now,
        );
        assert!(app.row_for_id(W1).is_none() && app.row_for_id(W2).is_none());
        for id in [W1, W2] {
            said(&mut state, &mut app, &brigade, id, b"x", now);
        }
        let title = |state: &EmporiumState, app: &App, id: &str| {
            let buffer = render_at(state, app, 160, now);
            status_segment(state, &buffer, id, 160)
        };

        said(&mut state, &mut app, &brigade, W1, REPORT_WORKING, now);
        assert_eq!(title(&state, &app, W1), "Codex \u{b7} busy");
        said(&mut state, &mut app, &brigade, W2, REPORT_BLOCKED, now);
        assert_eq!(title(&state, &app, W2), "", "not known to be Codex");
        assert!(!state.attention_panes().contains(&SessionKey::from_id(W2)));

        said(&mut state, &mut app, &brigade, W1, REPORT_BLOCKED, now);
        assert_eq!(title(&state, &app, W1), "Codex \u{b7} waiting");
        assert!(state.attention_panes().contains(&key), "marked");
        assert_eq!(state.visible_waiting_count(), 1, "counted");

        // The strip of the same pane says the same.
        toggle_minimize_for_test(&mut state, &mut app, &brigade, &key);
        let buffer = render_at(&state, &app, 160, now);
        assert_eq!(
            status_segment(&state, &buffer, W1, 160),
            "Codex \u{b7} waiting"
        );

        // The row turning up later changes nothing it shows.
        app.replace_rows(vec![SessionRow {
            activity: Activity::Alive,
            ..test_row(W1, AgentKind::Codex)
        }]);
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 160,
                height: 40,
            },
            now,
        );
        let buffer = render_at(&state, &app, 160, now);
        assert_eq!(
            status_segment(&state, &buffer, W1, 160),
            "Codex \u{b7} waiting"
        );
        assert_eq!(state.visible_waiting_count(), 1);
    }

    #[test]
    fn a_tile_being_moved_keeps_its_status_and_the_hit_tests_do_not_move() {
        use banto_core::input::{MouseButton, MouseEventKind};
        let (mut state, mut app, brigade, now) =
            scene(160, [Activity::Busy, Activity::Waiting, Activity::Alive]);
        let area = Rect::new(0, 0, 160, 40);
        let before = state.geometry(area);
        let status_before = status_segment(&state, &render_at(&state, &app, 160, now), W1, 160);
        assert_eq!(status_before, "Claude \u{b7} waiting");
        let (_, tile) = title_row(&state, &render_at(&state, &app, 160, now), W1, 160);

        // A press on the label marks the tile: its border and label take the
        // gesture's colour; the status, in its own colours, is still there.
        mouse(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            tile.x + 2,
            tile.y,
        );
        assert!(state.drag_overlay(area).is_some_and(|o| o.marked.is_some()));
        let pressed = render_at(&state, &app, 160, now);
        assert_eq!(pressed[(tile.x + 2, tile.y)].fg, DRAG_LINE_COLOR, "marked");
        assert_eq!(status_segment(&state, &pressed, W1, 160), status_before);
        let (row, _) = title_row(&state, &pressed, W1, 160);
        let word = row
            .chars()
            .collect::<Vec<_>>()
            .windows(8)
            .position(|w| w.iter().collect::<String>() == "waiting ")
            .expect("the word is on the title row") as u16;
        assert_eq!(
            pressed[(tile.x + word, tile.y)].fg,
            WAITING_ACTIVITY_COLOR,
            "the word keeps its own colour on the gesture's border"
        );

        // Nothing the gesture hit-tests has moved.
        let during = state.geometry(area);
        assert_eq!(during.buttons(), before.buttons());
        for column in tile.x..tile.x + tile.width {
            assert_eq!(
                during.label_at(column, tile.y),
                before.label_at(column, tile.y),
                "label at column {column}"
            );
            assert_eq!(
                during.button_at(column, tile.y),
                before.button_at(column, tile.y),
                "button at column {column}"
            );
        }

        // Released, it is as it was.
        mouse(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            tile.x + 2,
            tile.y,
        );
        let released = render_at(&state, &app, 160, now);
        assert_eq!(status_segment(&state, &released, W1, 160), status_before);
    }

    #[test]
    fn a_row_naming_another_product_replaces_the_reported_word_on_the_strip_and_the_title() {
        // worker-1 is Codex by the relay's facts and reports blocked. A row for
        // it arrives naming Claude: row and facts disagree, so it is not known to
        // be Codex, its report stops deciding, and both surfaces show the row's
        // own word, with the mark and count gone, in the update that brought the
        // row. The row going away, or agreeing, gives the report back.
        let (mut state, mut app, brigade) = staged_brigade_of_two_workers();
        let key = SessionKey::from_id(W1);
        let now = Instant::now();
        let update = |state: &mut EmporiumState, app: &mut App, event: Event| {
            engine::update(state, app, &BrigadeConfig::default(), event, now)
        };
        update(
            &mut state,
            &mut app,
            Event::Tick {
                relay: vec![RelayObservation {
                    brigade_id: 1,
                    token: "worker-1".to_string(),
                    key: key.clone(),
                    has_unseen: false,
                    is_idle_this_tick: None,
                    agent: Some(AgentKind::Codex),
                    live: None,
                    glance: None,
                    briefed_since_launch: None,
                }],
                brigades: Vec::new(),
            },
        );
        update(
            &mut state,
            &mut app,
            Event::Resized {
                width: 160,
                height: 40,
            },
        );
        said(&mut state, &mut app, &brigade, W1, REPORT_BLOCKED, now);
        let loaded = |rows: Vec<SessionRow>| Event::RowsLoaded {
            rows,
            hidden: HashSet::new(),
            directors: HashSet::new(),
        };
        let both = |state: &EmporiumState, app: &App| {
            let buffer = render_at(state, app, 160, now);
            let collapsed = state
                .geometry(Rect::new(0, 0, 160, 40))
                .is_collapsed(&SessionKey::from_id(W1));
            let title = status_segment(state, &buffer, W1, 160);
            // A collapsed tile's status is the same title row's.
            let strip = collapsed.then(|| title.clone());
            (title, strip)
        };
        let waiting = "Codex \u{b7} waiting";
        let claude_idle = "Claude \u{b7} idle";
        assert_eq!(both(&state, &app).0, waiting);
        assert!(state.attention_panes().contains(&key));
        assert_eq!(state.visible_waiting_count(), 1);

        // A row naming Claude arrives, and the shell's sample of it reads idle.
        update(
            &mut state,
            &mut app,
            Event::QuiescenceSampled {
                samples: vec![(key.clone(), Quiescence::Idle)],
            },
        );
        let claude = SessionRow {
            activity: Activity::Alive,
            ..test_row(W1, AgentKind::ClaudeCode)
        };
        let cmds = update(&mut state, &mut app, loaded(vec![claude.clone()]));
        assert_eq!(both(&state, &app).0, claude_idle, "the title");
        assert!(!state.attention_panes().contains(&key), "no mark");
        assert_eq!(state.visible_waiting_count(), 0, "no count");
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::RingHostBell { .. }))
        );
        // The strip of the same pane says the same.
        toggle_minimize_for_test(&mut state, &mut app, &brigade, &key);
        assert_eq!(both(&state, &app).1.as_deref(), Some(claude_idle));

        // The row goes: the kept report counts again, on the strip too.
        update(&mut state, &mut app, loaded(Vec::new()));
        assert_eq!(both(&state, &app).1.as_deref(), Some(waiting));
        assert!(state.attention_panes().contains(&key));
        // The row comes back agreeing: nothing changes.
        let codex = SessionRow {
            activity: Activity::Alive,
            ..test_row(W1, AgentKind::Codex)
        };
        update(&mut state, &mut app, loaded(vec![codex]));
        assert_eq!(both(&state, &app).1.as_deref(), Some(waiting));
        assert_eq!(state.visible_waiting_count(), 1);
    }

    #[test]
    fn a_claude_worker_with_no_row_yet_shows_what_its_live_file_says_on_both_surfaces() {
        // The operator saw a bare `Claude` with no word on a Worker added
        // minutes before: no transcript, so no row, but a live file.
        use banto_core::model::Activity::{Alive, Busy, Waiting};
        let (mut state, mut app, brigade) = staged_brigade_of_two_workers();
        let key = SessionKey::from_id(W1);
        let now = Instant::now();
        let observe = |state: &mut EmporiumState, app: &mut App, live: Option<LiveStatus>| {
            // The tick's sample of the same live file, as the shell reads it
            // (nothing observed, nothing sampled).
            let samples = live
                .map(|live| match live.activity {
                    Alive => Quiescence::Idle,
                    _ => Quiescence::NotIdle,
                })
                .map(|reading| vec![(SessionKey::from_id(W1), reading)])
                .unwrap_or_default();
            engine::update(
                state,
                app,
                &BrigadeConfig::default(),
                Event::QuiescenceSampled { samples },
                now,
            );
            engine::update(
                state,
                app,
                &BrigadeConfig::default(),
                Event::Tick {
                    relay: live
                        .map(|live| {
                            vec![RelayObservation {
                                brigade_id: 1,
                                token: "worker-1".to_string(),
                                key: SessionKey::from_id(W1),
                                has_unseen: false,
                                is_idle_this_tick: None,
                                agent: Some(AgentKind::ClaudeCode),
                                live: Some(live),
                                glance: None,
                                briefed_since_launch: None,
                            }]
                        })
                        .unwrap_or_default(),
                    brigades: Vec::new(),
                },
                now,
            )
        };
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 160,
                height: 40,
            },
            now,
        );
        said(&mut state, &mut app, &brigade, W1, b"x", now);
        assert!(app.row_for_id(W1).is_none());
        let title = |state: &EmporiumState, app: &App| {
            let buffer = render_at(state, app, 160, now);
            status_segment(state, &buffer, W1, 160)
        };
        // Nothing seen yet: no agent known, and no word.
        observe(&mut state, &mut app, None);
        assert_eq!(title(&state, &app), "");
        for (live, expected) in [
            (
                LiveStatus {
                    activity: Alive,
                    dialog_open: false,
                    status_busy: false,
                },
                "Claude \u{b7} idle",
            ),
            (
                LiveStatus {
                    activity: Busy,
                    dialog_open: false,
                    status_busy: true,
                },
                "Claude \u{b7} busy",
            ),
        ] {
            observe(&mut state, &mut app, Some(live));
            assert_eq!(title(&state, &app), expected, "{live:?}");
        }
        // Waiting: the word, the mark, and one in the count.
        let cmds = observe(
            &mut state,
            &mut app,
            Some(LiveStatus {
                activity: Waiting,
                dialog_open: false,
                status_busy: false,
            }),
        );
        assert_eq!(title(&state, &app), "Claude \u{b7} waiting");
        assert!(state.attention_panes().contains(&key), "marked");
        assert_eq!(state.visible_waiting_count(), 1);
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, Cmd::RingHostBell { .. })),
            "rung for"
        );
        // The strip says the same once it is collapsed.
        toggle_minimize_for_test(&mut state, &mut app, &brigade, &key);
        assert_eq!(title(&state, &app), "Claude \u{b7} waiting");
        // A tick that observed nothing leaves no word (the product it was
        // resolved as is kept, as before).
        observe(&mut state, &mut app, None);
        assert_eq!(title(&state, &app), "Claude");
        assert!(!state.attention_panes().contains(&key));
    }

    #[test]
    fn a_dialog_open_session_shows_as_waiting_on_the_title_and_the_strip() {
        // Told apart from a question only for the bell: on screen it is
        // Waiting, in the word, the colour and the tile's mark.
        let (mut state, mut app, brigade, now) =
            scene(160, [Activity::Alive, Activity::Waiting, Activity::Alive]);
        let rows = [
            (DIR, Activity::Alive, false),
            (W1, Activity::Waiting, true),
            (W2, Activity::Alive, false),
        ]
        .into_iter()
        .map(|(id, activity, dialog_open)| SessionRow {
            activity,
            dialog_open,
            ..test_row(
                id,
                if id == W2 {
                    AgentKind::Codex
                } else {
                    AgentKind::ClaudeCode
                },
            )
        })
        .collect();
        app.replace_rows(rows);
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 160,
                height: 40,
            },
            now,
        );
        let buffer = render_at(&state, &app, 160, now);
        assert_eq!(
            status_segment(&state, &buffer, W1, 160),
            "Claude \u{b7} waiting"
        );
        assert!(state.attention_panes().contains(&SessionKey::from_id(W1)));
        assert_eq!(state.visible_waiting_count(), 1);
        let key = SessionKey::from_id(W1);
        toggle_minimize_for_test(&mut state, &mut app, &brigade, &key);
        let buffer = render_at(&state, &app, 160, now);
        assert_eq!(
            status_segment(&state, &buffer, W1, 160),
            "Claude \u{b7} waiting"
        );
    }

    #[test]
    fn the_status_leaves_the_label_and_the_buttons_where_they_were() {
        let (state, app, _, now) = scene(160, [Activity::Busy, Activity::Waiting, Activity::Alive]);
        let buffer = render_at(&state, &app, 160, now);
        let geometry = state.geometry(Rect::new(0, 0, 160, 40));
        for (key, rect) in &geometry.tiles {
            let label = state.tile_label(key);
            let (start, end) = banto_core::layout::title_columns(*rect, &label);
            assert_eq!(
                text_at(&buffer, start, rect.y, end - start),
                banto_core::layout::title_text(&label),
                "the label of {}",
                key.as_str()
            );
            for button in geometry.buttons().iter().filter(|b| &b.key == key) {
                assert_eq!(
                    text_at(&buffer, button.rect.x, button.rect.y, button.rect.width),
                    button.text
                );
                // The press still lands on it.
                let hit = geometry.button_at(button.rect.x, button.rect.y).unwrap();
                assert_eq!((&hit.key, hit.kind), (key, button.kind));
            }
        }
        // The cell after the label and the cell before the first button are the
        // border's own.
        let (_, dir) = title_row(&state, &buffer, DIR, 160);
        let (_, label_end) =
            banto_core::layout::title_columns(dir, &state.tile_label(&SessionKey::from_id(DIR)));
        assert_eq!(text_at(&buffer, label_end, dir.y, 1), "\u{2500}");
    }

    #[test]
    fn a_collapsed_codex_tile_shows_what_the_pane_reports_not_the_rows_alive() {
        // The session list says "alive" for a live Codex session (its rollout
        // cannot say more); the pane's own report is what the tile must show.
        let codex = test_row(W2, AgentKind::Codex);
        assert_eq!(codex.activity, Activity::Alive);
        let (mut state, mut app, brigade) = staged_brigade_of_two_workers_with_rows(vec![codex]);
        let now = Instant::now();
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 160,
                height: 40,
            },
            now,
        );
        let w2_key = SessionKey::from_id(W2);
        said(&mut state, &mut app, &brigade, W2, b"x", now);
        said(&mut state, &mut app, &brigade, W2, REPORT_WORKING, now);
        toggle_minimize_for_test(&mut state, &mut app, &brigade, &w2_key);
        assert!(
            state
                .geometry(Rect::new(0, 0, 160, 40))
                .is_collapsed(&w2_key)
        );
        let status_row = |state: &EmporiumState, app: &App, word: &str| {
            let buffer = render_at(state, app, 160, now);
            (
                status_segment(state, &buffer, W2, 160),
                word_colour(state, &buffer, W2, 160, word),
            )
        };
        let (text, colour) = status_row(&state, &app, "busy");
        assert_eq!(text, "Codex \u{b7} busy");
        assert_eq!(colour, banto_tui::view::activity_color(Activity::Busy));

        said(&mut state, &mut app, &brigade, W2, REPORT_BLOCKED, now);
        let (text, colour) = status_row(&state, &app, "waiting");
        assert_eq!(text, "Codex \u{b7} waiting");
        assert_eq!(colour, WAITING_ACTIVITY_COLOR);

        said(
            &mut state,
            &mut app,
            &brigade,
            W2,
            b"\x1b]7501;state=clear\x1b\\",
            now,
        );
        // Nothing reported any more, and nothing read: no word. The sample the
        // shell takes next decides (the row's own "alive" never says it).
        let buffer = render_at(&state, &app, 160, now);
        assert_eq!(status_segment(&state, &buffer, W2, 160), "Codex");
        sampled(
            &mut state,
            &mut app,
            &brigade,
            &[(W2, Quiescence::Idle)],
            now,
        );
        assert_eq!(status_row(&state, &app, "idle").0, "Codex \u{b7} idle");
    }

    #[test]
    fn a_collapsed_tile_is_a_title_row_and_two_body_rows_and_shows_waiting() {
        use banto_core::input::{MouseButton, MouseEventKind};
        let mut waiting = test_row("new-worker::1::worker-1", AgentKind::ClaudeCode);
        waiting.activity = Activity::Waiting;
        let mut busy = test_row("new-worker::1::worker-2", AgentKind::Codex);
        busy.activity = Activity::Busy;
        let (mut state, mut app, brigade) =
            staged_brigade_of_two_workers_with_rows(vec![waiting, busy]);
        let area = Rect::new(0, 0, 120, 40);
        let button_of = |state: &EmporiumState, name: &str, kind: ButtonKind| {
            state
                .geometry(area)
                .buttons()
                .iter()
                .find(|b| b.key.as_str().ends_with(name) && b.kind == kind)
                .cloned()
                .unwrap()
        };
        let w2_key = SessionKey::from_id("new-worker::1::worker-2");
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key: w2_key.clone(),
                chunk: b"hello".to_vec(),
            },
            Instant::now(),
        );

        // Collapse worker-1 with its own button.
        let minimize = button_of(&state, "worker-1", ButtonKind::Minimize);
        mouse(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            minimize.rect.x,
            minimize.rect.y,
        );
        let geometry = state.geometry(area);
        let strip = geometry.tiles[1].1;
        assert!(geometry.is_collapsed(&geometry.tiles[1].0));
        assert_eq!(strip.height, banto_core::engine::COLLAPSED_TILE_HEIGHT);

        let buffer = render(&state, &app);
        // Title row: the label, then the buttons ending before the corner.
        assert_eq!(text_at(&buffer, strip.x + 1, strip.y, 10), " worker-1 ");
        let right = strip.x + strip.width - 1;
        assert_eq!(text_at(&buffer, right - 6, strip.y, 6), "[S][+]");
        // The status is on that same row: the activity word, in the waiting
        // colour, and the agent before it, which a 42-column tile has room for.
        assert_eq!(
            status_segment(&state, &buffer, W1, 120),
            "Claude \u{b7} waiting"
        );
        assert_eq!(
            word_colour(&state, &buffer, W1, 120, "waiting"),
            WAITING_ACTIVITY_COLOR
        );
        // The two body rows are the messages, blank here, and then the border.
        for y in [strip.y + 1, strip.y + 2] {
            assert_eq!(text_at(&buffer, strip.x + 1, y, 30).trim_end(), "");
        }
        // A tile waiting for the operator is the one case he must not miss:
        // the collapsed tile's border shows it.
        for (x, y) in [
            (strip.x, strip.y + 2),
            (strip.x + 7, strip.y + strip.height - 1),
            (right, strip.y + 2),
        ] {
            assert_eq!(
                buffer[(x, y)].fg,
                WAITING_ACTIVITY_COLOR,
                "border cell ({x},{y})"
            );
        }

        // The expanded tile beside it: its buttons say [_], its [S] is plain
        // until it is soloed, then reversed.
        let w2 = state.geometry(area).tiles[2].1;
        let w2_right = w2.x + w2.width - 1;
        assert_eq!(text_at(&buffer, w2_right - 6, w2.y, 6), "[S][_]");
        assert!(
            !buffer[(w2_right - 6, w2.y)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        let solo = button_of(&state, "worker-2", ButtonKind::Solo);
        mouse(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            solo.rect.x,
            solo.rect.y,
        );
        let soloed = render(&state, &app);
        assert!(
            soloed[(w2_right - 6, w2.y)]
                .modifier
                .contains(Modifier::REVERSED),
            "[S] reversed"
        );
        assert!(
            !soloed[(w2_right - 3, w2.y)]
                .modifier
                .contains(Modifier::REVERSED),
            "[_] is not"
        );
        assert_eq!(text_at(&soloed, w2_right - 3, w2.y, 3), "[_]");
    }

    // --- message glances: store -> gate -> Event -> state -> strip ---------------

    #[test]
    fn the_gate_reads_a_member_once_then_again_only_when_a_message_is_appended() {
        let store = Store::open_in_memory().unwrap();
        let brigade = store.create_brigade("cell").unwrap();
        store
            .enqueue_brigade_message(brigade, "director", BrigadeRole::Worker, None, "first")
            .unwrap();
        let mut gate = GlanceGate::default();
        let read = |gate: &mut GlanceGate, store: &Store, token: &str| {
            gate.read(
                store,
                worker_of(brigade, token, "session-a"),
                store.max_brigade_message_id().ok(),
                false,
            )
        };

        let first = read(&mut gate, &store, "worker-1").expect("never read: read now");
        assert_eq!(first.inbound.map(|g| g.line), Some("first".to_string()));
        assert_eq!(
            read(&mut gate, &store, "worker-1"),
            None,
            "nothing new: nothing read"
        );
        // Another member has its own record of what it has read.
        assert!(read(&mut gate, &store, "worker-2").is_some());
        assert_eq!(read(&mut gate, &store, "worker-2"), None);

        store
            .enqueue_brigade_message(brigade, "director", BrigadeRole::Worker, None, "second")
            .unwrap();
        let again = read(&mut gate, &store, "worker-1").expect("a message arrived");
        assert_eq!(again.inbound.map(|g| g.line), Some("second".to_string()));
        assert!(
            read(&mut gate, &store, "worker-2").is_some(),
            "and for everyone"
        );

        // Without a newest id (the lookup failed) it always reads: a blank
        // tile on a transient error is the worse failure.
        let blind = |gate: &mut GlanceGate| {
            gate.read(
                &store,
                worker_of(brigade, "worker-1", "session-a"),
                None,
                false,
            )
        };
        assert!(blind(&mut gate).is_some());
        assert!(blind(&mut gate).is_some());

        // A new member under a reused token (the Goinkyo's is reused by each
        // consultation; a Worker's never is in its brigade, but a hand-edited
        // store could) is read afresh even though no message has been appended
        // since the old one was.
        let _ = read(&mut gate, &store, "worker-1");
        assert_eq!(read(&mut gate, &store, "worker-1"), None);
        let newest = store.max_brigade_message_id().ok();
        assert!(
            gate.read(
                &store,
                worker_of(brigade, "worker-1", "session-b"),
                newest,
                false
            )
            .is_some()
        );
    }

    #[test]
    fn the_gate_forgets_dismissed_members_and_disbanded_brigades_and_rereads_a_returner() {
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        let claude_home = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut gate = GlanceGate::default();
        let gather = |gate: &mut GlanceGate| {
            gather_relay_observations(&state, &store, &home, None, &app, None, gate)
        };
        let remembered = |gate: &GlanceGate| {
            let mut ids: Vec<String> = gate
                .read_at
                .keys()
                .map(|(_, token)| token.clone())
                .collect();
            ids.sort();
            ids
        };

        gather(&mut gate);
        assert_eq!(remembered(&gate), ["director", "worker-1"]);
        // Settled: only the Director, who still has unseen mail, is read again.
        assert!(
            gather(&mut gate)
                .iter()
                .all(|o| o.glance.is_none() == !o.has_unseen),
            "settled"
        );

        // The Worker is dismissed: its entry goes on the next tick.
        store.borrow_mut().dismiss_worker(1, "worker-1").unwrap();
        gather(&mut gate);
        assert_eq!(remembered(&gate), ["director"]);

        // It comes back under the same token and session: read again, though
        // no message has been appended since.
        store
            .borrow_mut()
            .add_brigade_member(
                1,
                "worker-1",
                BrigadeRole::Worker,
                Some(&SessionId("w1".to_string())),
            )
            .unwrap();
        let back = gather(&mut gate);
        let worker = back.iter().find(|o| o.token == "worker-1").unwrap();
        assert!(worker.glance.is_some(), "a returning member is read afresh");
        assert_eq!(remembered(&gate), ["director", "worker-1"]);

        // The brigade is disbanded: nothing is left.
        store.borrow_mut().delete_brigade(1).unwrap();
        gather(&mut gate);
        assert!(gate.read_at.is_empty());
        assert!(
            gate.visited.is_empty(),
            "and no visit is carried over a tick"
        );
    }

    fn worker_of<'a>(brigade: BrigadeId, token: &'a str, session: &'a str) -> GlanceMember<'a> {
        GlanceMember {
            brigade_id: brigade,
            token,
            session_id: session,
            role: BrigadeRole::Worker,
        }
    }

    #[test]
    fn a_member_with_unseen_mail_has_its_inbound_glance_read_every_tick_whatever_the_ids_say() {
        let store = Store::open_in_memory().unwrap();
        let brigade = store.create_brigade("cell").unwrap();
        let send = |from: &str, to_role, body: &str| {
            store
                .enqueue_brigade_message(brigade, from, to_role, None, body)
                .unwrap()
        };
        send("director", BrigadeRole::Worker, "first");
        send("worker-1", BrigadeRole::Director, "reply");
        let newest = store.max_brigade_message_id().ok();
        let mut gate = GlanceGate::default();
        let read = |gate: &mut GlanceGate, has_unseen| {
            gate.read(
                &store,
                worker_of(brigade, "worker-1", "s"),
                newest,
                has_unseen,
            )
        };

        let first = read(&mut gate, true).expect("never read");
        assert_eq!(
            first.inbound.as_ref().map(|g| g.line.as_str()),
            Some("first")
        );
        assert_eq!(first.sent.as_ref().map(|g| g.line.as_str()), Some("reply"));

        // The very same newest id: nothing unseen, the gate still skips...
        assert_eq!(read(&mut gate, false), None);
        // ...but with unseen mail the inbound half is read again, and picks up
        // a message the stale `newest` does not know about.
        send("director", BrigadeRole::Worker, "landed just now");
        let again = read(&mut gate, true).expect("unseen mail: read, ids equal or not");
        assert_eq!(
            again.inbound.as_ref().map(|g| g.line.as_str()),
            Some("landed just now")
        );
        // The sent half came from what the gate kept, not from a second query.
        assert_eq!(again.sent, first.sent);
        // And it is still skipped once nothing is unseen. (The stale id: a
        // message that landed mid-gather is read next tick, when the id moves.)
        assert_eq!(read(&mut gate, false), None);
        let moved = store.max_brigade_message_id().ok();
        assert!(
            gate.read(&store, worker_of(brigade, "worker-1", "s"), moved, false)
                .is_some(),
            "the id moved: read afresh"
        );
        // The recorded id after an unseen read is the tick's own, so it cannot
        // be newer than what that read saw.
        assert_eq!(
            gate.read_at
                .get(&(brigade, "worker-1".to_string()))
                .map(|read| read.newest_message),
            moved
        );
    }

    #[test]
    fn the_relay_gather_carries_each_visited_brigades_name_and_director_session() {
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        // A second brigade, with a Director that has no session id yet, and a
        // third whose roster has no Director at all.
        {
            let mut store = store.borrow_mut();
            let second = store.create_brigade("Second task").unwrap();
            store
                .add_brigade_member(second, "director", BrigadeRole::Director, None)
                .unwrap();
            store.create_brigade("Empty roster").unwrap();
        }
        let claude_home = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut gathered = GlanceGate::default();

        gather_relay_observations(&state, &store, &home, None, &app, None, &mut gathered);
        let facts = gathered.brigades;

        // `staged_worker` made brigade 1 named "cell" with Director session "dir".
        let by_id = |id| facts.iter().find(|f| f.brigade_id == id).cloned();
        assert_eq!(
            by_id(1),
            Some(BrigadeNameFacts {
                brigade_id: 1,
                name: "cell".to_string(),
                director_session: Some("dir".to_string()),
                member_names: Vec::new(),
                request: None,
                interrupts: Some(Vec::new()),
            })
        );
        assert_eq!(
            by_id(2),
            Some(BrigadeNameFacts {
                brigade_id: 2,
                name: "Second task".to_string(),
                director_session: None,
                member_names: Vec::new(),
                request: None,
                interrupts: Some(Vec::new()),
            })
        );
        assert_eq!(by_id(3).map(|f| f.director_session), Some(None));
        assert_eq!(facts.len(), 3, "every visited brigade, once");
    }

    #[test]
    fn a_tick_carries_each_brigades_open_request_from_the_store_and_nothing_a_director_wrote() {
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        let other = store.borrow_mut().create_brigade("other").unwrap();
        store.borrow_mut().set_request(1, "director").unwrap();
        store.borrow_mut().set_request(1, "director").unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut gathered = GlanceGate::default();
        gather_relay_observations(&state, &store, &home, None, &app, None, &mut gathered);
        let request_of = |id| {
            gathered
                .brigades
                .iter()
                .find(|facts| facts.brigade_id == id)
                .map(|facts| facts.request.clone())
        };
        assert_eq!(
            request_of(1),
            Some(Some(banto_core::engine::RequestFact { seq: 2 }))
        );
        assert_eq!(request_of(other), Some(None), "a brigade with none says so");
        // The fact is a sequence and nothing else: a recording of the tick holds no
        // word of a Director's through this path.
        let json = serde_json::to_string(&gathered.brigades).unwrap();
        assert!(json.contains("\"request\":{\"seq\":2}"), "{json}");
    }

    #[test]
    fn answering_clears_the_store_only_while_it_is_still_that_request() {
        let (store, _, _) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        store.borrow_mut().set_request(1, "director").unwrap();
        store.borrow_mut().set_request(1, "director").unwrap();
        let clear =
            |seq| execute_store_intent(StoreIntent::ClearRequest { brigade_id: 1, seq }, &store);
        assert!(clear(1).is_empty(), "no event comes back");
        assert_eq!(
            store.borrow().brigade_request(1).unwrap(),
            Some(2),
            "the older answer wiped nothing"
        );
        clear(2);
        assert_eq!(store.borrow().brigade_request(1).unwrap(), None);
    }

    /// The brigade's tick reads an open request, as the store would hand it over:
    /// through a real tick.
    fn give_request(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        open: bool,
        now: Instant,
    ) {
        engine::update(
            state,
            app,
            brigade,
            Event::Tick {
                relay: Vec::new(),
                brigades: vec![BrigadeNameFacts {
                    brigade_id: 1,
                    name: "cell".to_string(),
                    director_session: Some(DIR.to_string()),
                    member_names: Vec::new(),
                    request: open.then_some(banto_core::engine::RequestFact { seq: 1 }),
                    interrupts: None,
                }],
            },
            now,
        );
    }

    /// The title row of the Director's tile, drawn `width` columns wide, with an
    /// open request or none.
    fn director_request_title(width: u16, open: bool) -> String {
        let (mut state, mut app, brigade, now) =
            scene(width, [Activity::Busy, Activity::Alive, Activity::Alive]);
        give_request(&mut state, &mut app, &brigade, open, now);
        let buffer = render_at(&state, &app, width, now);
        title_row(&state, &buffer, DIR, width).0
    }

    #[test]
    fn a_staged_directors_title_row_carries_the_tag_beside_the_state_word() {
        use banto_tui::strip::REQUEST_TAG;
        for width in [160u16, 120] {
            let title = director_request_title(width, true);
            assert!(
                title.contains(REQUEST_TAG),
                "{width}: the tag is whole: {title:?}"
            );
            assert!(title.contains("busy"), "{width}: the word stays: {title:?}");
            assert!(title.contains("director"), "{width}: the label: {title:?}");
            assert!(title.contains("[S]"), "{width}: the buttons: {title:?}");
        }
        // At 100 columns the Director's tile gives its title row about 14 cells, and
        // `busy · ▸ decision` needs 17: the tag stands alone, whole, with the label
        // and the buttons in place.
        let title = director_request_title(100, true);
        assert!(title.contains(REQUEST_TAG), "{title:?}");
        assert!(!title.contains("busy"), "{title:?}");
        assert!(
            title.contains("director") && title.contains("[S]"),
            "{title:?}"
        );
        // Without a request the title row is what it was.
        let plain = director_request_title(160, false);
        assert!(!plain.contains('\u{25b8}'), "{plain:?}");
    }

    #[test]
    fn the_directors_tile_shows_the_tag_while_its_pane_is_focused_and_a_workers_never_does() {
        use banto_tui::strip::REQUEST_TAG;
        let (mut state, mut app, brigade, now) =
            scene(160, [Activity::Alive, Activity::Alive, Activity::Alive]);
        give_request(&mut state, &mut app, &brigade, true, now);
        assert_eq!(state.focus, Focus::Pane, "the Director's pane is focused");
        let buffer = render_at(&state, &app, 160, now);
        assert!(title_row(&state, &buffer, DIR, 160).0.contains(REQUEST_TAG));
        for worker in [W1, W2] {
            assert!(
                !title_row(&state, &buffer, worker, 160)
                    .0
                    .contains('\u{25b8}'),
                "{worker}"
            );
        }
    }

    #[test]
    fn a_bell_for_a_request_is_logged_with_its_brigade_and_director() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(dir.path().join("bell.log"))
            .unwrap();
        let cause = banto_core::engine::BellCause::Request {
            brigade: 3,
            director: SessionKey::from_id("dir"),
        };
        ring_host_bell_to(
            &mut Vec::new(),
            &cause,
            Some(BellEntry {
                file: &mut file,
                since_start: Duration::from_millis(900),
                event: "Tick",
            }),
        );
        let lines = log_lines(&mut file);
        assert!(
            lines[0]
                .ends_with("bell +900ms cause=request brigade=3 director=dir event=Tick written"),
            "{lines:?}"
        );
    }

    #[test]
    fn a_gather_whose_brigade_list_fails_hands_back_no_facts_from_an_earlier_tick() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let mut store = Store::open(&db).unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "director",
                BrigadeRole::Director,
                Some(&SessionId("dir".to_string())),
            )
            .unwrap();
        let store = RefCell::new(store);
        let claude_home = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = App::new(vec![test_row("dir", AgentKind::ClaudeCode)]);
        let config = BrigadeConfig::default();
        let now = Instant::now();
        let mut gate = GlanceGate::default();

        // A tick that read the brigade: its name facts reach the core.
        let relay = gather_relay_observations(&state, &store, &home, None, &app, None, &mut gate);
        assert_eq!(gate.brigades.len(), 1);
        engine::update(
            &mut state,
            &mut app,
            &config,
            Event::Tick {
                relay,
                brigades: gate.brigades.clone(),
            },
            now,
        );
        assert_ne!(
            state.brigade_name(&app, brigade_id),
            format!("brigade {brigade_id}"),
            "the core knows the brigade's name"
        );

        // The brigade is disbanded; the core forgets its name at once.
        store.borrow_mut().delete_brigade(brigade_id).unwrap();
        engine::update(
            &mut state,
            &mut app,
            &config,
            Event::Disbanded {
                brigade_id,
                result: Ok((HashSet::new(), HashSet::new())),
            },
            now,
        );
        assert_eq!(
            state.brigade_name(&app, brigade_id),
            format!("brigade {brigade_id}")
        );

        // The next gather cannot list brigades at all (the table is gone from
        // under it), and the facts the gate still holds from the tick before —
        // nothing took them — must not be handed back.
        let other = rusqlite::Connection::open(&db).unwrap();
        other
            .execute_batch("ALTER TABLE brigades RENAME TO brigades_gone")
            .unwrap();
        assert!(
            store.borrow().list_brigades().is_err(),
            "the failure is real"
        );
        let relay = gather_relay_observations(&state, &store, &home, None, &app, None, &mut gate);
        assert!(relay.is_empty());
        assert!(gate.brigades.is_empty(), "nothing stale is forwarded");
        engine::update(
            &mut state,
            &mut app,
            &config,
            Event::Tick {
                relay,
                brigades: std::mem::take(&mut gate.brigades),
            },
            now,
        );
        assert_eq!(
            state.brigade_name(&app, brigade_id),
            format!("brigade {brigade_id}"),
            "the disbanded brigade's name stays gone"
        );
    }

    /// What the loop does with a gather's samples: hand them to the core.
    fn dispatch_samples(
        state: &mut EmporiumState,
        app: &mut App,
        gate: &mut GlanceGate,
        now: Instant,
    ) {
        let samples = std::mem::take(&mut gate.quiescence);
        engine::update(
            state,
            app,
            &BrigadeConfig::default(),
            Event::QuiescenceSampled { samples },
            now,
        );
    }

    /// A session read idle on a good tick, then a tick whose roster read fails
    /// while its live file says `status_after`; returns the app and the bar after
    /// one quit key.
    fn quit_key_after_a_failed_roster_read(status_after: &str) -> (App, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let mut store = Store::open(&db).unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "director",
                BrigadeRole::Director,
                Some(&SessionId("dir".to_string())),
            )
            .unwrap();
        let store = RefCell::new(store);
        let claude_home = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = App::new(vec![
            test_row("dir", AgentKind::ClaudeCode),
            test_row("solo", AgentKind::ClaudeCode),
        ]);
        let now = Instant::now();
        state
            .screens
            .insert(SessionKey::from_id("solo"), Screen::new(24, 80));
        state.focus = Focus::Sidebar;
        let mut gate = GlanceGate::default();

        // A good tick: the session reads idle.
        write_live_state_with_status(
            claude_home.path(),
            std::process::id(),
            "solo",
            Path::new("/work"),
            "idle",
        );
        gather_relay_observations(&state, &store, &home, None, &app, None, &mut gate);
        dispatch_samples(&mut state, &mut app, &mut gate, now);

        // The next tick cannot list brigades (the table is gone from under it),
        // and in the meantime the session started a turn.
        let other = rusqlite::Connection::open(&db).unwrap();
        other
            .execute_batch("ALTER TABLE brigades RENAME TO brigades_gone")
            .unwrap();
        assert!(
            store.borrow().list_brigades().is_err(),
            "the failure is real"
        );
        write_live_state_with_status(
            claude_home.path(),
            std::process::id(),
            "solo",
            Path::new("/work"),
            status_after,
        );
        gather_relay_observations(&state, &store, &home, None, &app, None, &mut gate);
        dispatch_samples(
            &mut state,
            &mut app,
            &mut gate,
            now + Duration::from_secs(1),
        );

        let bar = press_esc(&mut state, &mut app, now + Duration::from_secs(5));
        (app, bar)
    }

    #[test]
    fn a_tick_that_cannot_read_the_roster_does_not_leave_an_old_idle_standing() {
        // The session started a turn meanwhile: one press must not quit on the
        // idle it was read as before.
        let (app, bar) = quit_key_after_a_failed_roster_read("busy");
        assert!(!app.should_quit(), "{bar:?}");
        assert!(bar.contains("1 not idle"), "{bar:?}");
    }

    #[test]
    fn a_session_that_can_still_be_read_keeps_its_answer_through_a_failed_roster_read() {
        // Its live file does not depend on the roster, so it is not made
        // unknown for nothing.
        let (app, bar) = quit_key_after_a_failed_roster_read("idle");
        assert!(app.should_quit(), "{bar:?}");
    }

    #[test]
    fn what_needed_the_roster_to_be_understood_reads_unknown_until_a_tick_that_can_read_it() {
        // A Codex Worker whose product only the store records (no session row):
        // with the roster unreadable nothing says it is a Codex session, so the
        // tick cannot call it idle, and the next good tick can again.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let mut store = Store::open(&db).unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "worker-1",
                BrigadeRole::Worker,
                Some(&SessionId("w1".to_string())),
            )
            .unwrap();
        store
            .set_member_agent(brigade_id, "worker-1", AgentKind::Codex)
            .unwrap();
        let store = RefCell::new(store);
        let claude_home = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let codex_dir = tempfile::tempdir().unwrap();
        write_codex_rollout(codex_dir.path(), "w1", &["task_started", "task_complete"]);
        let codex_home = CodexHome::new(codex_dir.path().to_path_buf());
        let mut state = EmporiumState::new(PrefixKey::default());
        state
            .screens
            .insert(SessionKey::from_id("w1"), Screen::new(24, 80));
        let mut app = App::new(Vec::new());
        let now = Instant::now();
        let other = rusqlite::Connection::open(&db).unwrap();
        let sample_at = |state: &mut EmporiumState, app: &mut App, at: Instant| {
            let mut gate = GlanceGate::default();
            gather_relay_observations(
                state,
                &store,
                &home,
                Some(&codex_home),
                app,
                None,
                &mut gate,
            );
            dispatch_samples(state, app, &mut gate, at);
        };

        sample_at(&mut state, &mut app, now);

        other
            .execute_batch("ALTER TABLE brigades RENAME TO brigades_gone")
            .unwrap();
        assert!(store.borrow().list_brigades().is_err());
        sample_at(&mut state, &mut app, now + Duration::from_secs(1));
        state.focus = Focus::Sidebar;
        let bar = press_esc(&mut state, &mut app, now + Duration::from_secs(5));
        assert!(!app.should_quit(), "{bar:?}");
        assert!(bar.contains("1 not idle"), "{bar:?}");

        // The roster is back: the next tick reads it again, and one press
        // quits again.
        other
            .execute_batch("ALTER TABLE brigades_gone RENAME TO brigades")
            .unwrap();
        sample_at(&mut state, &mut app, now + Duration::from_secs(6));
        let bar = press_esc(&mut state, &mut app, now + Duration::from_secs(20));
        assert!(app.should_quit(), "{bar:?}");
    }

    #[test]
    fn the_relay_gather_carries_each_members_glances_without_consuming_the_mail() {
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        let claude_home = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut gate = GlanceGate::default();
        let gather = |gate: &mut GlanceGate| {
            gather_relay_observations(&state, &store, &home, None, &app, None, gate)
        };

        let observations = gather(&mut gate);
        let worker = observations.iter().find(|o| o.token == "worker-1").unwrap();
        let director = observations.iter().find(|o| o.token == "director").unwrap();
        // `staged_worker` queued "hi" from worker-1 to the Director.
        let worker_glance = worker.glance.as_ref().expect("first read");
        assert_eq!(
            worker_glance.sent.as_ref().map(|g| g.line.as_str()),
            Some("hi")
        );
        assert_eq!(worker_glance.inbound, None);
        let director_glance = director.glance.as_ref().expect("first read");
        assert_eq!(
            director_glance.inbound.as_ref().map(|g| g.line.as_str()),
            Some("hi")
        );
        assert_eq!(director_glance.sent, None);
        assert!(
            worker.agent.is_some(),
            "the resolved product travels with it"
        );

        // The next tick with no new message reads nothing for a member with
        // nothing unseen, and the inbound half again for the Director, whose
        // mail is still unread (and whose sent half is what the gate kept).
        let quiet = gather(&mut gate);
        assert!(
            quiet
                .iter()
                .find(|o| o.token == "worker-1")
                .unwrap()
                .glance
                .is_none()
        );
        assert_eq!(
            quiet.iter().find(|o| o.token == "director").unwrap().glance,
            director.glance
        );
        // Reading was not receiving: the Director still has unseen mail.
        assert!(director.has_unseen);
        assert!(
            store
                .borrow()
                .has_unseen_brigade_messages(1, "director", BrigadeRole::Director)
                .unwrap()
        );

        // A new message is picked up on the next tick.
        store
            .borrow()
            .enqueue_brigade_message(
                1,
                "director",
                BrigadeRole::Worker,
                None,
                "\u{1b}[31mdo it\nnow",
            )
            .unwrap();
        let after = gather(&mut gate);
        let worker = after.iter().find(|o| o.token == "worker-1").unwrap();
        assert_eq!(
            worker
                .glance
                .as_ref()
                .and_then(|g| g.inbound.as_ref())
                .map(|g| g.line.as_str()),
            Some("[31mdo it"),
            "reduced to its first line with the escape removed"
        );
    }

    #[test]
    fn a_member_that_joined_after_a_broadcast_does_not_show_it_as_its_last_message() {
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        let claude_home = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        // A broadcast to the Workers, queued while only worker-1 existed...
        store
            .borrow()
            .enqueue_brigade_message(1, "director", BrigadeRole::Worker, None, "before worker-2")
            .unwrap();
        // ...and a second Worker that joins after it, hosted in a pane of its own.
        let second = store
            .borrow_mut()
            .add_worker(1, Some(AgentKind::ClaudeCode))
            .unwrap();
        assert_eq!(second, "worker-2");
        store
            .borrow_mut()
            .set_member_session(1, &second, &SessionId("w2".to_string()))
            .unwrap();
        let mut state = state;
        state
            .screens
            .insert(SessionKey::from_id("w2"), Screen::new(24, 80));

        let inbound_line = |observations: &[RelayObservation], token: &str| {
            observations
                .iter()
                .find(|o| o.token == token)
                .unwrap_or_else(|| panic!("{token} was not observed"))
                .glance
                .as_ref()
                .and_then(|g| g.inbound.as_ref())
                .map(|g| g.line.clone())
        };

        // The first tick of a fresh gate reads both members. Worker-2 has no mail
        // of its own yet; worker-1 still shows the broadcast it was there for.
        let mut gate = GlanceGate::default();
        let first = gather_relay_observations(&state, &store, &home, None, &app, None, &mut gate);
        let new_member = first.iter().find(|o| o.token == "worker-2").unwrap();
        assert_eq!(
            new_member.glance.as_ref().map(|g| g.inbound.clone()),
            Some(None),
            "read, and nothing was addressed to it after it joined"
        );
        assert_eq!(
            inbound_line(&first, "worker-1"),
            Some("before worker-2".to_string())
        );

        // The next tick: nothing new for worker-2 (no message appended, nothing
        // unseen, same session), so the gate reads nothing and hands the core no
        // glance at all. That is different from a read that found nothing, which
        // the first tick and the fresh gate below assert; a stale pre-join line
        // could only come out of a read, and would be a `Some` here.
        let second_tick =
            gather_relay_observations(&state, &store, &home, None, &app, None, &mut gate);
        assert!(
            second_tick
                .iter()
                .find(|o| o.token == "worker-2")
                .unwrap()
                .glance
                .is_none(),
            "the gate skipped the read: it kept what the first tick found"
        );
        assert_eq!(
            inbound_line(&second_tick, "worker-1"),
            Some("before worker-2".to_string())
        );

        // A gate that remembers nothing reads it again with the same answer.
        let mut fresh_gate = GlanceGate::default();
        let fresh =
            gather_relay_observations(&state, &store, &home, None, &app, None, &mut fresh_gate);
        assert_eq!(
            fresh
                .iter()
                .find(|o| o.token == "worker-2")
                .unwrap()
                .glance
                .as_ref()
                .map(|g| g.inbound.clone()),
            Some(None),
            "read again from the store, and still nothing before the join"
        );
        assert_eq!(
            inbound_line(&fresh, "worker-1"),
            Some("before worker-2".to_string())
        );

        // A broadcast sent after the join reaches both tiles.
        store
            .borrow()
            .enqueue_brigade_message(1, "director", BrigadeRole::Worker, None, "after worker-2")
            .unwrap();
        let after = gather_relay_observations(&state, &store, &home, None, &app, None, &mut gate);
        assert_eq!(
            inbound_line(&after, "worker-2"),
            Some("after worker-2".to_string())
        );
        assert_eq!(
            inbound_line(&after, "worker-1"),
            Some("after worker-2".to_string())
        );
    }

    #[test]
    fn a_collapsed_tiles_message_rows_come_through_an_observation_and_clip_by_width() {
        use banto_core::input::{MouseButton, MouseEventKind};
        let (mut state, mut app, brigade) = staged_brigade_of_two_workers();
        let area = Rect::new(0, 0, 120, 40);
        let key = SessionKey::from_id("new-worker::1::worker-1");
        let observation = |glance: Option<MemberGlance>, agent| RelayObservation {
            brigade_id: 1,
            token: "worker-1".to_string(),
            key: key.clone(),
            has_unseen: false,
            is_idle_this_tick: None,
            agent,
            live: None,
            glance,
            briefed_since_launch: None,
        };
        let now_ms = i64::try_from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation(
                    Some(MemberGlance {
                        inbound: Some(MessageGlance {
                            created_at_ms: now_ms - 5 * 60 * 1000,
                            line: "run the whole suite and report".to_string(),
                            from_token: Some("director".to_string()),
                        }),
                        sent: Some(MessageGlance {
                            created_at_ms: now_ms,
                            line: "\u{30c6}\u{30b9}\u{30c8}".repeat(40),
                            from_token: Some("worker-1".to_string()),
                        }),
                    }),
                    Some(AgentKind::Codex),
                )],
                brigades: Vec::new(),
            },
            Instant::now(),
        );
        let minimize = state
            .geometry(area)
            .buttons()
            .iter()
            .find(|b| b.key == key && b.kind == ButtonKind::Minimize)
            .cloned()
            .unwrap();
        mouse(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            minimize.rect.x,
            minimize.rect.y,
        );
        let strip = state.geometry(area).tiles[1].1;
        let inner = strip.width - 2;

        let buffer = render(&state, &app);

        // The title row says what is known of a pane the list has no row for and
        // that has written nothing: the product the relay resolved, and no word.
        assert_eq!(status_segment(&state, &buffer, W1, 120), "Codex");
        // The two body rows are the messages, the first row the inbound one.
        let inbound = text_at(&buffer, strip.x + 1, strip.y + 1, inner);
        assert!(
            inbound.starts_with("\u{2190} 5m run the whole suite and report"),
            "{inbound:?}"
        );
        let sent = text_at(&buffer, strip.x + 1, strip.y + 2, inner);
        // A wide character's second cell reads as a space in the buffer.
        let compact: String = sent.chars().filter(|c| *c != ' ').collect();
        assert!(
            compact.starts_with("\u{2192}now\u{30c6}\u{30b9}\u{30c8}"),
            "{sent:?}"
        );
        // Wide text is cut by display width at the content edge, ellipsis
        // included, and nothing spills onto the border.
        assert!(compact.ends_with('\u{2026}'), "{sent:?}");
        let right = strip.x + strip.width - 1;
        assert_eq!(buffer[(right, strip.y + 2)].symbol(), "\u{2502}");

        // An observation that re-read nothing leaves the rows as they are.
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation(None, None)],
                brigades: Vec::new(),
            },
            Instant::now(),
        );
        let buffer = render(&state, &app);
        assert!(
            text_at(&buffer, strip.x + 1, strip.y + 1, 20).starts_with("\u{2190} 5m run the whole")
        );
    }

    #[test]
    fn the_master_tile_has_a_solo_button_drawn_reversed_while_its_solo_is_active() {
        use banto_core::input::{MouseButton, MouseEventKind};
        let (mut state, mut app, brigade) = staged_brigade_of_two_workers();
        let area = Rect::new(0, 0, 120, 40);
        let master = state.geometry(area).tiles[0].1;
        let right = master.x + master.width - 1;

        let before = render(&state, &app);
        assert_eq!(text_at(&before, right - 3, master.y, 3), "[S]");
        // The label ends well before it: the cells between are plain border.
        assert_eq!(text_at(&before, right - 4, master.y, 1), "\u{2500}");
        assert!(
            !before[(right - 3, master.y)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        // Solo only: no collapse button on the master.
        assert!(!text_at(&before, master.x + 1, master.y, master.width - 2).contains("[_]"));

        let button = state
            .geometry(area)
            .buttons()
            .iter()
            .find(|b| b.key == state.geometry(area).tiles[0].0)
            .cloned()
            .unwrap();
        mouse(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            button.rect.x,
            button.rect.y,
        );

        assert!(state.soloed_key().is_some(), "the press soloed the master");
        // The master is wider now (the stack narrowed), so its button moved.
        let master = state.geometry(area).tiles[0].1;
        let right = master.x + master.width - 1;
        let after = render(&state, &app);
        assert!(
            after[(right - 3, master.y)]
                .modifier
                .contains(Modifier::REVERSED),
            "[S] reversed"
        );
        assert_eq!(text_at(&after, right - 3, master.y, 3), "[S]");
        // Soloing the master collapsed every stack tile: their buttons read [+].
        let stack = state.geometry(area).tiles[1].1;
        let stack_right = stack.x + stack.width - 1;
        assert_eq!(text_at(&after, stack_right - 3, stack.y, 3), "[+]");
    }

    // --- arrangement persistence, end to end --------------------------------------

    /// Feed `events` through the core and keep going on what comes back: store
    /// intents are executed against `store` (and their answers fed in), every
    /// open is answered with a spawn. Returns every command seen.
    fn drive(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        store: &RefCell<Store>,
        events: Vec<Event>,
    ) -> Vec<Cmd> {
        let mut seen = Vec::new();
        let mut queue: VecDeque<Event> = events.into();
        while let Some(event) = queue.pop_front() {
            for cmd in engine::update(state, app, brigade, event, Instant::now()) {
                match &cmd {
                    Cmd::Store(intent) => queue.extend(execute_store_intent(intent.clone(), store)),
                    Cmd::OpenEmbedded { key, .. } => queue.push_back(Event::Spawned {
                        key: key.clone(),
                        agent: None,
                    }),
                    _ => {}
                }
                seen.push(cmd);
            }
        }
        seen
    }

    fn key_event(code: banto_core::input::KeyCode, mods: banto_core::input::Modifiers) -> Event {
        Event::Input(InputEvent::Key(banto_core::input::KeyEvent::new(
            code, mods,
        )))
    }

    /// A brigade of the Director and two Workers, all with session rows, in a
    /// store file; returns the app and the store.
    fn stored_brigade_in(path: &std::path::Path) -> (App, RefCell<Store>) {
        let mut store = Store::open(path).unwrap();
        let br = store.create_brigade("cell").unwrap();
        assert_eq!(br, 1);
        for (token, role, session) in [
            ("director", BrigadeRole::Director, "dir"),
            ("worker-1", BrigadeRole::Worker, "w1"),
            ("worker-2", BrigadeRole::Worker, "w2"),
        ] {
            store
                .add_brigade_member(br, token, role, Some(&SessionId(session.to_string())))
                .unwrap();
        }
        let app = App::new(vec![
            test_row("dir", AgentKind::ClaudeCode),
            test_row("w1", AgentKind::ClaudeCode),
            test_row("w2", AgentKind::ClaudeCode),
        ]);
        (app, RefCell::new(store))
    }

    /// One "run" of the emporium over `store`: nothing but what the store
    /// holds, the sidebar width fed first as the real loop does, the brigade
    /// staged by activating its Director.
    fn start_run(app: &mut App, brigade: &BrigadeConfig, store: &RefCell<Store>) -> EmporiumState {
        let mut state = EmporiumState::new(PrefixKey::default());
        let width = store.borrow().sidebar_width().unwrap();
        drive(
            &mut state,
            app,
            brigade,
            store,
            vec![
                Event::SidebarWidthLoaded { width },
                Event::Resized {
                    width: 120,
                    height: 40,
                },
                key_event(
                    banto_core::input::KeyCode::Enter,
                    banto_core::input::Modifiers::NONE,
                ),
            ],
        );
        state
    }

    /// Tiles by name, collapsed flags, the pane area, the pane order.
    type Picture = (Vec<(String, Rect)>, Vec<bool>, Rect, Vec<String>);

    fn picture(state: &EmporiumState) -> Picture {
        let geometry = state.geometry(Rect::new(0, 0, 120, 40));
        (
            geometry
                .tiles
                .iter()
                .map(|(key, rect)| (key.as_str().to_string(), *rect))
                .collect(),
            geometry.collapsed.clone(),
            geometry.areas.pane,
            match &state.stage {
                Stage::Brigade { panes, .. } => {
                    panes.iter().map(|k| k.as_str().to_string()).collect()
                }
                _ => Vec::new(),
            },
        )
    }

    #[test]
    fn an_arrangement_survives_a_restart_through_a_real_store() {
        use banto_core::engine::Boundary;
        use banto_core::input::{KeyCode, Modifiers, MouseButton, MouseEventKind};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("banto.db");
        let brigade = BrigadeConfig::default();
        let area = Rect::new(0, 0, 120, 40);

        // --- first run: stage the brigade, then arrange it
        let (mut app, store) = stored_brigade_in(&path);
        let mut state = start_run(&mut app, &brigade, &store);
        let plain = picture(&state);
        assert_eq!(plain.3, ["dir", "w1", "w2"], "staged in member order");

        let press = |state: &mut EmporiumState, app: &mut App, kind, column, row| {
            drive(
                state,
                app,
                &brigade,
                &store,
                vec![Event::Input(InputEvent::Mouse(
                    banto_core::input::MouseEvent { kind, column, row },
                ))],
            );
        };
        let drag = |state: &mut EmporiumState, app: &mut App, boundary: Boundary, by: i32| {
            let handle = state
                .geometry(area)
                .handle_for(&boundary)
                .cloned()
                .expect("the boundary exists");
            let hit = handle.hits()[0];
            let (column, row) = (hit.x + 2.min(hit.width - 1), hit.y);
            let target = |from: u16| (i32::from(from) + by) as u16;
            let (to_column, to_row) = match handle.axis {
                Axis::Vertical => (target(column), row),
                Axis::Horizontal => (column, target(row)),
            };
            press(
                state,
                app,
                MouseEventKind::Down(MouseButton::Left),
                column,
                row,
            );
            press(
                state,
                app,
                MouseEventKind::Drag(MouseButton::Left),
                to_column,
                to_row,
            );
            press(
                state,
                app,
                MouseEventKind::Up(MouseButton::Left),
                to_column,
                to_row,
            );
        };
        // The sidebar, the master edge, a row edge.
        drag(&mut state, &mut app, Boundary::Sidebar, 10);
        drag(&mut state, &mut app, Boundary::Master { brigade: 1 }, 5);
        let w1 = SessionKey::from_id("w1");
        let w2 = SessionKey::from_id("w2");
        drag(
            &mut state,
            &mut app,
            Boundary::Stack {
                brigade: 1,
                above: w1.clone(),
                below: w2.clone(),
            },
            4,
        );
        // Reorder from the keyboard: focus w2, shift-up after the prefix.
        let prefix_then = |state: &mut EmporiumState, app: &mut App, code, mods| {
            drive(
                state,
                app,
                &brigade,
                &store,
                vec![
                    key_event(KeyCode::Char('b'), Modifiers::CONTROL),
                    key_event(code, mods),
                ],
            );
        };
        prefix_then(&mut state, &mut app, KeyCode::Char('3'), Modifiers::NONE);
        prefix_then(&mut state, &mut app, KeyCode::Up, Modifiers::SHIFT);
        // Collapse the tile now second (w1), from its button.
        let minimize = state
            .geometry(area)
            .buttons()
            .iter()
            .find(|b| b.key == w1 && b.kind == ButtonKind::Minimize)
            .cloned()
            .unwrap();
        press(
            &mut state,
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            minimize.rect.x,
            minimize.rect.y,
        );
        let arranged = picture(&state);
        assert_ne!(arranged, plain, "the arrangement changed the picture");
        assert_eq!(arranged.3, ["dir", "w2", "w1"]);
        assert_eq!(arranged.1, [false, false, true]);

        // --- a second run from nothing but the file
        drop(state);
        drop(store);
        let (mut app2, store2) = {
            let store = Store::open(&path).unwrap();
            (
                App::new(vec![
                    test_row("dir", AgentKind::ClaudeCode),
                    test_row("w1", AgentKind::ClaudeCode),
                    test_row("w2", AgentKind::ClaudeCode),
                ]),
                RefCell::new(store),
            )
        };
        let restarted = start_run(&mut app2, &brigade, &store2);

        assert_eq!(
            picture(&restarted),
            arranged,
            "the same screen after a restart"
        );
        assert_eq!(
            restarted.geometry(area).areas.pane.x,
            46,
            "the sidebar width came back"
        );
    }

    #[test]
    fn restoring_everything_is_stored_too_so_a_restart_returns_to_the_defaults() {
        use banto_core::input::{KeyCode, Modifiers};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("banto.db");
        let brigade = BrigadeConfig::default();
        let (mut app, store) = stored_brigade_in(&path);
        let mut state = start_run(&mut app, &brigade, &store);
        let plain = picture(&state);
        let chord = |state: &mut EmporiumState, app: &mut App, code, mods| {
            drive(
                state,
                app,
                &brigade,
                &store,
                vec![
                    key_event(KeyCode::Char('b'), Modifiers::CONTROL),
                    key_event(code, mods),
                ],
            );
        };
        chord(&mut state, &mut app, KeyCode::Char('3'), Modifiers::NONE);
        chord(&mut state, &mut app, KeyCode::Char('m'), Modifiers::NONE);
        assert_ne!(picture(&state), plain);
        chord(&mut state, &mut app, KeyCode::Char('M'), Modifiers::SHIFT);
        assert_eq!(picture(&state), plain);

        let (mut app2, store2) = (
            App::new(vec![
                test_row("dir", AgentKind::ClaudeCode),
                test_row("w1", AgentKind::ClaudeCode),
                test_row("w2", AgentKind::ClaudeCode),
            ]),
            RefCell::new(Store::open(&path).unwrap()),
        );
        let restarted = start_run(&mut app2, &brigade, &store2);
        assert_eq!(
            picture(&restarted),
            plain,
            "the cleared flag was written, not just forgotten"
        );
    }

    #[test]
    fn a_collapsed_tile_shows_no_output_age_however_long_the_pane_has_been_quiet() {
        let (mut state, mut app, brigade, now) =
            scene(120, [Activity::Alive, Activity::Alive, Activity::Alive]);
        toggle_minimize_for_test_at(
            &mut state,
            &mut app,
            &brigade,
            &SessionKey::from_id(W1),
            120,
        );
        for quiet in [0, 25, 130, 7300] {
            let buffer = render_at(&state, &app, 120, now + Duration::from_secs(quiet));
            assert_eq!(
                status_segment(&state, &buffer, W1, 120),
                "Claude \u{b7} idle",
                "{quiet}s after the pane wrote"
            );
        }
    }

    // Unix only: on Windows crossterm sets the mouse mode through the console
    // API, not bytes, and fails on a buffer ("Initial console modes not set").
    #[cfg(unix)]
    #[test]
    fn the_restore_sequence_shows_the_cursor_on_the_main_screen() {
        let mut written = Vec::new();
        write_restore_sequence(&mut written).unwrap();
        let written = String::from_utf8(written).unwrap();
        let leave = written
            .find("\x1b[?1049l")
            .expect("leaves the alternate screen");
        let show = written.find("\x1b[?25h").expect("shows the cursor");
        assert!(leave < show, "{written:?}");
    }

    #[test]
    fn a_drag_line_reaching_past_the_buffer_is_cut_to_it() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 3));
        paint_drag_line(&mut buffer, Rect::new(3, 0, 1, 20), Axis::Vertical);
        paint_drag_line(&mut buffer, Rect::new(9, 9, 1, 5), Axis::Vertical);
        assert_eq!(buffer[(3, 2)].symbol(), "\u{2503}");
    }

    #[derive(Default)]
    struct ProbeBackend {
        calls: CellRef<Vec<&'static str>>,
    }

    impl ProbeBackend {
        fn call(&self, name: &'static str) {
            self.calls.borrow_mut().push(name);
        }
    }

    impl Backend for ProbeBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, _content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            self.call("draw");
            std::thread::sleep(Duration::from_millis(1));
            Ok(())
        }

        fn append_lines(&mut self, _n: u16) -> Result<(), Self::Error> {
            self.call("append_lines");
            Ok(())
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.call("hide_cursor");
            Ok(())
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.call("show_cursor");
            Ok(())
        }

        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.call("get_cursor_position");
            Ok(Position::new(3, 4))
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            _position: P,
        ) -> Result<(), Self::Error> {
            self.call("set_cursor_position");
            Ok(())
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            self.call("clear");
            Ok(())
        }

        fn clear_region(&mut self, _clear_type: ClearType) -> Result<(), Self::Error> {
            self.call("clear_region");
            Ok(())
        }

        fn size(&self) -> Result<ratatui::layout::Size, Self::Error> {
            self.call("size");
            Ok(ratatui::layout::Size::new(80, 24))
        }

        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.call("window_size");
            Ok(WindowSize {
                columns_rows: ratatui::layout::Size::new(80, 24),
                pixels: ratatui::layout::Size::new(0, 0),
            })
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            self.call("flush");
            Ok(())
        }
    }

    #[test]
    fn timed_backend_passes_every_backend_call_through_and_resets_per_sample() {
        let timings = Rc::new(CellRef::new(BackendTimings::default()));
        let mut backend = TimedBackend::new(ProbeBackend::default(), timings).unwrap();
        let _ = backend.take_timings();
        backend
            .draw(std::iter::empty::<(u16, u16, &ratatui::buffer::Cell)>())
            .unwrap();
        backend.append_lines(1).unwrap();
        backend.hide_cursor().unwrap();
        backend.show_cursor().unwrap();
        assert_eq!(backend.get_cursor_position().unwrap(), Position::new(3, 4));
        backend.set_cursor_position(Position::new(5, 6)).unwrap();
        backend.clear().unwrap();
        backend.clear_region(ClearType::CurrentLine).unwrap();
        assert_eq!(backend.size().unwrap(), ratatui::layout::Size::new(80, 24));
        assert_eq!(backend.window_size().unwrap().columns_rows.width, 80);
        backend.flush().unwrap();

        let sample = backend.take_timings();
        assert!(sample.draw.elapsed >= Duration::from_millis(1));
        assert_eq!(sample.draw.calls, 1);
        assert_eq!(sample.append_lines.calls, 1);
        assert_eq!(sample.hide_cursor.calls, 1);
        assert_eq!(sample.show_cursor.calls, 1);
        assert_eq!(sample.get_cursor_position.calls, 1);
        assert_eq!(sample.set_cursor_position.calls, 1);
        assert_eq!(sample.clear.calls, 1);
        assert_eq!(sample.clear_region.calls, 1);
        assert_eq!(sample.size.calls, 1);
        assert_eq!(sample.window_size.calls, 1);
        assert_eq!(sample.flush.calls, 1);
        assert_eq!(
            backend.inner.calls.borrow().clone(),
            vec![
                "size",
                "draw",
                "append_lines",
                "hide_cursor",
                "show_cursor",
                "get_cursor_position",
                "set_cursor_position",
                "clear",
                "clear_region",
                "window_size",
                "flush",
            ]
        );
        let reset = backend.take_timings();
        assert_eq!(reset.draw.calls, 0);
        assert_eq!(reset.draw.elapsed, Duration::ZERO);
    }

    #[test]
    fn frame_writer_batches_writes_and_counts_successful_bytes() {
        let timings = Rc::new(CellRef::new(BackendTimings::default()));
        let (mut writer, mut output) = FrameWriter::new(Vec::new(), timings.clone());
        writer.write_all(b"abc").unwrap();
        writer.flush().unwrap();
        assert!(output.has_pending());
        output.present().unwrap();
        assert_eq!(output.output.borrow().inner, b"abc");
        assert_eq!(timings.borrow().bytes_written, 3);
    }

    #[derive(Default)]
    struct RecordingWriter {
        writes: Vec<Vec<u8>>,
        flushes: usize,
    }

    impl io::Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes.push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn frame_writer_presents_screen_cursor_and_hyperlink_close_in_one_write() {
        let timings = Rc::new(CellRef::new(BackendTimings::default()));
        let (mut writer, mut output) = FrameWriter::new(RecordingWriter::default(), timings);
        writer.write_all(b"screen").unwrap();
        writer.write_all(b"cursor").unwrap();
        output.write_hyperlink_close().unwrap();
        output.present().unwrap();

        let output = output.output.borrow();
        assert_eq!(
            output.inner.writes,
            vec![b"screencursor\x1b]8;;\x1b\\".to_vec()]
        );
        assert_eq!(output.inner.flushes, 1);
    }

    #[test]
    fn cached_size_avoids_per_frame_console_queries_and_refreshes_on_demand() {
        let timings = Rc::new(CellRef::new(BackendTimings::default()));
        let mut backend = TimedBackend::new(ProbeBackend::default(), timings).unwrap();
        assert_eq!(backend.inner.calls.borrow().as_slice(), ["size"]);

        assert_eq!(backend.size().unwrap(), Size::new(80, 24));
        backend.set_cached_size(Size::new(120, 40));
        assert_eq!(backend.size().unwrap(), Size::new(120, 40));
        assert_eq!(backend.inner.calls.borrow().as_slice(), ["size"]);

        assert_eq!(backend.refresh_size().unwrap(), Size::new(80, 24));
        assert_eq!(backend.inner.calls.borrow().as_slice(), ["size", "size"]);
    }

    #[test]
    fn identical_frame_skips_but_a_cell_or_cursor_change_presents() {
        let buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        let presented = PresentedFrame {
            buffer: buffer.clone(),
            cursor: None,
        };
        assert!(!should_present(Some(&presented), &buffer, None));

        let mut changed_cell = buffer.clone();
        changed_cell[(0, 0)].set_symbol("x");
        assert!(should_present(Some(&presented), &changed_cell, None));
        assert!(should_present(
            Some(&presented),
            &buffer,
            Some(Position::new(1, 0))
        ));
    }

    #[test]
    fn a_draw_dominant_slow_iteration_logs_backend_breakdown() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut file = Some(
            std::fs::OpenOptions::new()
                .append(true)
                .open(temp.path())
                .unwrap(),
        );
        let timings = StageTimings {
            pty_pump: Duration::ZERO,
            discovery: Duration::ZERO,
            reload: Duration::ZERO,
            tick: Duration::ZERO,
            dispatch: Duration::ZERO,
            draw: Duration::from_millis(20),
        };
        let draw = DrawTimings {
            assemble: Duration::from_millis(3),
            hyperlink_close: Duration::from_micros(7),
            hyperlink_bytes: 7,
            skipped: false,
            backend: BackendTimings {
                draw: MethodTimings {
                    calls: 1,
                    elapsed: Duration::from_millis(16),
                },
                bytes_written: 123,
                ..BackendTimings::default()
            },
        };
        log_slow_iteration(&mut file, &timings, &TickTimings::default(), &draw);
        drop(file);
        let logged = std::fs::read_to_string(temp.path()).unwrap();
        assert!(logged.contains("emporium: slow draw skipped=false assemble=3000us hyperlink_close=7us hyperlink_bytes=7 bytes=123 backend_draw=16000us/1"));
        assert!(logged.contains("size=0us/0 window_size=0us/0 flush=0us/0"));
    }

    fn open(host: &MockPtyHost) -> PtyHandle {
        PtyHandle::open(host, &["child".to_string()], None, &[], &[], 24, 80).unwrap()
    }

    // --- whether a Codex member was briefed since its pane's launch ---------

    fn roster_member(token: &str, briefed_at: Option<SystemTime>) -> BrigadeMember {
        BrigadeMember {
            token: token.to_string(),
            role: BrigadeRole::Worker,
            session_id: None,
            agent: None,
            briefed_at,
            briefed_session_id: None,
            display_name: None,
        }
    }

    #[test]
    fn a_briefing_counts_only_from_the_moment_the_process_was_spawned() {
        let spawned = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let second = Duration::from_secs(1);
        let members = [
            roster_member("never", None),
            roster_member("before", Some(spawned - second)),
            roster_member("at", Some(spawned)),
            roster_member("after", Some(spawned + second)),
        ];
        for (token, expected) in [
            ("never", Some(false)),
            ("before", Some(false)),
            ("at", Some(true)),
            ("after", Some(true)),
            ("nobody", None),
        ] {
            assert_eq!(
                briefed_since_launch(&members, token, spawned),
                expected,
                "{token}"
            );
        }
    }

    /// A tick's read holding one brigade with `members`, or a roster that could
    /// not be read.
    fn read_with(members: Option<Vec<BrigadeMember>>) -> TickRead {
        TickRead {
            brigades: members.map(|members| {
                vec![(
                    Brigade {
                        id: 1,
                        name: "cell".to_string(),
                    },
                    Some(members),
                )]
            }),
            live: Vec::new(),
            alive: std::cell::OnceCell::new(),
        }
    }

    /// A hosted pane for each of `keys`, whose process was spawned a moment ago.
    fn hosted(keys: &[&str]) -> HashMap<SessionKey, PtyHandle> {
        let host = MockPtyHost::default();
        keys.iter()
            .map(|key| (SessionKey::from_id(key), open(&host)))
            .collect()
    }

    fn observation_of(token: &str, key: &str, agent: AgentKind) -> RelayObservation {
        RelayObservation {
            brigade_id: 1,
            token: token.to_string(),
            key: SessionKey::from_id(key),
            has_unseen: false,
            is_idle_this_tick: None,
            agent: Some(agent),
            glance: None,
            live: None,
            briefed_since_launch: None,
        }
    }

    #[test]
    fn a_codex_observation_is_told_whether_its_row_was_briefed_since_the_pane_spawned() {
        let handles = hosted(&["fresh", "stale", "never"]);
        let spawned = |key: &str| handles[&SessionKey::from_id(key)].spawned_at();
        let read = read_with(Some(vec![
            roster_member("fresh", Some(spawned("fresh") + Duration::from_secs(1))),
            roster_member("stale", Some(spawned("stale") - Duration::from_secs(1))),
            roster_member("never", None),
        ]));
        let mut relay = vec![
            observation_of("fresh", "fresh", AgentKind::Codex),
            observation_of("stale", "stale", AgentKind::Codex),
            observation_of("never", "never", AgentKind::Codex),
        ];

        note_briefed_since_launch(&mut relay, &read, &handles);

        let told: Vec<Option<bool>> = relay.iter().map(|obs| obs.briefed_since_launch).collect();
        assert_eq!(told, [Some(true), Some(false), Some(false)]);
    }

    #[test]
    fn nothing_is_told_for_a_claude_member_a_missing_pane_or_an_unread_roster() {
        let handles = hosted(&["claude"]);
        let read = read_with(Some(vec![
            roster_member("claude", None),
            roster_member("no-pane", None),
        ]));
        let mut relay = vec![
            observation_of("claude", "claude", AgentKind::ClaudeCode),
            observation_of("no-pane", "no-pane", AgentKind::Codex),
        ];
        note_briefed_since_launch(&mut relay, &read, &handles);
        assert!(relay.iter().all(|obs| obs.briefed_since_launch.is_none()));

        let unread = read_with(None);
        let mut relay = vec![observation_of("claude", "claude", AgentKind::Codex)];
        note_briefed_since_launch(&mut relay, &unread, &handles);
        assert_eq!(relay[0].briefed_since_launch, None);
    }

    /// Every product this build supports — the "nothing restricted" `Deps`
    /// field most tests here want, mirroring `session::tests::all_agents`.
    fn all_agents() -> BTreeSet<AgentKind> {
        AgentKind::ALL.into_iter().collect()
    }

    #[test]
    fn shutdown_sweep_lets_a_promptly_exiting_child_go_without_a_force_kill() {
        let kills = Arc::new(Mutex::new(0));
        let host = MockPtyHost {
            kills: kills.clone(),
            ..Default::default()
        };
        let mut handles = HashMap::new();
        handles.insert(SessionKey::from_id("a"), open(&host));
        host.fire_exit();

        shutdown_handles(
            &mut handles,
            Duration::from_millis(200),
            Duration::from_millis(5),
        );

        assert_eq!(*kills.lock().unwrap(), 0);
    }

    #[test]
    fn shutdown_sweep_force_kills_a_child_that_outlives_the_deadline() {
        let kills = Arc::new(Mutex::new(0));
        let host = MockPtyHost {
            kills: kills.clone(),
            ..Default::default()
        };
        let mut handles = HashMap::new();
        handles.insert(SessionKey::from_id("a"), open(&host));
        // Never fired: this child never exits on its own.

        shutdown_handles(
            &mut handles,
            Duration::from_millis(20),
            Duration::from_millis(5),
        );

        assert_eq!(*kills.lock().unwrap(), 1);
    }

    #[test]
    fn shutdown_sweep_shares_one_deadline_across_every_child() {
        let kills_a = Arc::new(Mutex::new(0));
        let kills_b = Arc::new(Mutex::new(0));
        let host_a = MockPtyHost {
            kills: kills_a.clone(),
            ..Default::default()
        };
        let host_b = MockPtyHost {
            kills: kills_b.clone(),
            ..Default::default()
        };
        let mut handles = HashMap::new();
        handles.insert(SessionKey::from_id("a"), open(&host_a));
        handles.insert(SessionKey::from_id("b"), open(&host_b));
        host_a.fire_exit(); // a exits promptly; b never does.

        let start = Instant::now();
        shutdown_handles(
            &mut handles,
            Duration::from_millis(50),
            Duration::from_millis(5),
        );

        assert!(
            start.elapsed() < Duration::from_millis(500),
            "one shared deadline, not one per child"
        );
        assert_eq!(*kills_a.lock().unwrap(), 0, "a exited on its own");
        assert_eq!(*kills_b.lock().unwrap(), 1, "b outlived the deadline");
    }

    /// Name a member the way the Director's `name_member` tool does.
    fn name_it(
        store: &mut Store,
        br: BrigadeId,
        token: &str,
        name: &str,
    ) -> banto_io::store::MemberNaming {
        store.name_member(br, token, name).unwrap()
    }

    #[test]
    fn a_name_the_director_gives_reaches_the_tile_through_the_store_and_a_tick() {
        a_name_reaches_the_tile_through_the_store(true);
    }

    #[test]
    fn a_name_reaches_the_tile_of_a_worker_that_has_no_session_id_yet() {
        // The common moment to name a Worker: freshly spawned, no transcript, so
        // the roster has no session id for it and the gather makes no observation
        // of it — while its tile is on screen under the synthetic key.
        a_name_reaches_the_tile_through_the_store(false);
    }

    fn a_name_reaches_the_tile_through_the_store(workers_have_a_session: bool) {
        // Two connections to one file, as the emporium and a Director's MCP
        // server are: the name is written by the one and read by the other.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let mut mcp_side = Store::open(&db).unwrap();
        let brigade_id = mcp_side.create_brigade("cell").unwrap();
        // The staged brigade's panes are keyed `new-worker::1::worker-N` until
        // discovery; a member whose recorded session is that key is the pane.
        mcp_side
            .add_brigade_member(
                brigade_id,
                "director",
                BrigadeRole::Director,
                Some(&SessionId("dir".to_string())),
            )
            .unwrap();
        for _ in 0..2 {
            let token = mcp_side
                .add_worker(brigade_id, Some(AgentKind::ClaudeCode))
                .unwrap();
            if workers_have_a_session {
                mcp_side
                    .set_member_session(
                        brigade_id,
                        &token,
                        &SessionId(format!("new-worker::1::{token}")),
                    )
                    .unwrap();
            }
        }
        let store = RefCell::new(Store::open(&db).unwrap());
        assert_eq!(brigade_id, 1);

        let (mut state, mut app, _) = staged_brigade_of_two_workers();
        let config = BrigadeConfig::default();
        let claude_home = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut gate = GlanceGate::default();
        let worker = SessionKey::from_id("new-worker::1::worker-1");
        let tick = |state: &mut EmporiumState, app: &mut App, gate: &mut GlanceGate| {
            let relay = gather_relay_observations(state, &store, &home, None, app, None, gate);
            // Whether the worker has an observation at all depends on its session.
            assert_eq!(
                relay.iter().any(|o| o.token == "worker-1"),
                workers_have_a_session
            );
            let brigades = std::mem::take(&mut gate.brigades);
            let named = brigades
                .iter()
                .flat_map(|facts| facts.member_names.iter())
                .find(|(token, _)| token == "worker-1")
                .map(|(_, name)| name.clone());
            let cmds = engine::update(
                state,
                app,
                &config,
                Event::Tick { relay, brigades },
                Instant::now(),
            );
            (named, cmds)
        };

        let (named, _) = tick(&mut state, &mut app, &mut gate);
        assert_eq!(named, None);
        assert_eq!(state.tile_label(&worker), "worker-1");

        // The Director's `name_member` writes through its own connection.
        assert!(matches!(
            name_it(&mut mcp_side, brigade_id, "worker-1", "impl"),
            banto_io::store::MemberNaming::Set { .. }
        ));
        let (named, cmds) = tick(&mut state, &mut app, &mut gate);
        assert_eq!(named.as_deref(), Some("impl"), "the gather read it");
        assert_eq!(state.tile_label(&worker), "impl (worker-1)");
        assert!(
            !cmds.iter().any(|cmd| matches!(
                cmd,
                Cmd::ResizePty { .. } | Cmd::Store(StoreIntent::SaveBrigadeLayout { .. })
            )),
            "a rename is no resize and no layout write: {cmds:?}"
        );

        // Taken back: the tile follows within a tick.
        name_it(&mut mcp_side, brigade_id, "worker-1", "");
        tick(&mut state, &mut app, &mut gate);
        assert_eq!(state.tile_label(&worker), "worker-1");
    }

    #[test]
    fn resolve_membership_carries_the_named_members_only() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        for (token, role, session) in [
            ("director", BrigadeRole::Director, "dir"),
            ("worker-1", BrigadeRole::Worker, "w1"),
            ("worker-2", BrigadeRole::Worker, "w2"),
        ] {
            store
                .add_brigade_member(
                    brigade_id,
                    token,
                    role,
                    Some(&SessionId(session.to_string())),
                )
                .unwrap();
        }
        name_it(&mut store, brigade_id, "worker-2", "docs");
        let store = RefCell::new(store);

        let events = execute_store_intent(
            StoreIntent::ResolveMembership {
                session_id: "dir".to_string(),
            },
            &store,
        );
        let [Event::MembershipResolved { names, members, .. }] = events.as_slice() else {
            panic!("a resolved membership: {events:?}");
        };
        assert_eq!(names, &[("worker-2".to_string(), "docs".to_string())]);
        assert_eq!(members.as_ref().map(Vec::len), Some(3));
    }

    #[test]
    fn resolve_membership_hands_the_roster_over_in_the_order_the_members_joined() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        // The Goinkyo joined after both Workers; by token it would sort first.
        for (token, role, session) in [
            ("director", BrigadeRole::Director, "dir"),
            ("worker-1", BrigadeRole::Worker, "w1"),
            ("worker-2", BrigadeRole::Worker, "w2"),
            ("goinkyo", BrigadeRole::Goinkyo, "g1"),
        ] {
            store
                .add_brigade_member(
                    brigade_id,
                    token,
                    role,
                    Some(&SessionId(session.to_string())),
                )
                .unwrap();
        }
        let store = RefCell::new(store);

        let events = execute_store_intent(
            StoreIntent::ResolveMembership {
                session_id: "dir".to_string(),
            },
            &store,
        );

        let [
            Event::MembershipResolved {
                members: Some(members),
                ..
            },
        ] = events.as_slice()
        else {
            panic!("expected a resolved membership with a roster: {events:?}");
        };
        let tokens: Vec<&str> = members.iter().map(|(token, ..)| token.as_str()).collect();
        assert_eq!(tokens, ["director", "worker-1", "worker-2", "goinkyo"]);
    }

    #[test]
    fn resolve_membership_leaves_an_up_to_date_session_id_untouched() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "worker-1",
                BrigadeRole::Worker,
                Some(&SessionId("w1".to_string())),
            )
            .unwrap();
        let store = RefCell::new(store);

        let events = execute_store_intent(
            StoreIntent::ResolveMembership {
                session_id: "w1".to_string(),
            },
            &store,
        );

        let [
            Event::MembershipResolved {
                members: Some(members),
                ..
            },
        ] = events.as_slice()
        else {
            panic!("expected a resolved membership with a roster: {events:?}");
        };
        let worker = members
            .iter()
            .find(|(token, ..)| token == "worker-1")
            .unwrap();
        assert_eq!(worker.2.as_deref(), Some("w1"));
    }

    #[test]
    fn resolve_membership_tolerates_a_worker_still_awaiting_discovery() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "director",
                BrigadeRole::Director,
                Some(&SessionId("dir".to_string())),
            )
            .unwrap();
        store
            .add_brigade_member(brigade_id, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        let store = RefCell::new(store);

        let events = execute_store_intent(
            StoreIntent::ResolveMembership {
                session_id: "dir".to_string(),
            },
            &store,
        );

        let [
            Event::MembershipResolved {
                members: Some(members),
                ..
            },
        ] = events.as_slice()
        else {
            panic!("expected a resolved membership with a roster: {events:?}");
        };
        let worker = members
            .iter()
            .find(|(token, ..)| token == "worker-1")
            .unwrap();
        assert_eq!(worker.2, None, "nothing to heal for an unassigned member");
    }

    // --- dismiss a Worker (暇を出す) --------------------------------------

    #[test]
    fn dismiss_worker_store_intent_removes_membership_and_reports_refreshed_sets() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "director",
                BrigadeRole::Director,
                Some(&SessionId("dir".to_string())),
            )
            .unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "worker-1",
                BrigadeRole::Worker,
                Some(&SessionId("w1".to_string())),
            )
            .unwrap();
        let store = RefCell::new(store);

        let events = execute_store_intent(
            StoreIntent::DismissWorker {
                brigade_id,
                token: "worker-1".to_string(),
            },
            &store,
        );

        let [
            Event::WorkerDismissed {
                brigade_id: reported_id,
                result: Ok((hidden, directors)),
            },
        ] = events.as_slice()
        else {
            panic!("expected a successful WorkerDismissed: {events:?}");
        };
        assert_eq!(*reported_id, brigade_id);
        // w1 left the hidden-worker set (dismissed for good, honestly
        // surfaced as an ordinary session); the director is untouched.
        assert!(!hidden.contains("w1"));
        assert!(directors.contains("dir"));
        assert_eq!(
            store
                .borrow()
                .brigade_member(brigade_id, "worker-1")
                .unwrap(),
            None
        );
    }

    #[test]
    fn formation_and_add_worker_share_one_counter_and_a_dismissed_number_is_not_handed_back() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let (brigade_id, tokens) = form_brigade_store(
            &store,
            "dir",
            AgentKind::ClaudeCode,
            "cell",
            2,
            AgentKind::ClaudeCode,
        )
        .unwrap();
        assert_eq!(tokens, ["worker-1", "worker-2"]);

        // The highest is dismissed: the next Worker is not worker-2 again.
        store
            .borrow_mut()
            .dismiss_worker(brigade_id, "worker-2")
            .unwrap();
        assert_eq!(
            add_worker_store(&store, brigade_id, AgentKind::ClaudeCode).unwrap(),
            "worker-3"
        );
        // Every Worker dismissed: numbering goes on, it does not restart at 1.
        for token in ["worker-1", "worker-3"] {
            store
                .borrow_mut()
                .dismiss_worker(brigade_id, token)
                .unwrap();
        }
        assert_eq!(
            add_worker_store(&store, brigade_id, AgentKind::Codex).unwrap(),
            "worker-4"
        );
        // The product recorded for the new row is the one asked for.
        assert_eq!(
            store
                .borrow()
                .brigade_member(brigade_id, "worker-4")
                .unwrap()
                .unwrap()
                .agent,
            Some(AgentKind::Codex)
        );
        // A second brigade formed afterwards starts at 1.
        let (other, tokens) = form_brigade_store(
            &store,
            "dir2",
            AgentKind::ClaudeCode,
            "other",
            1,
            AgentKind::ClaudeCode,
        )
        .unwrap();
        assert_ne!(other, brigade_id);
        assert_eq!(tokens, ["worker-1"]);
    }

    #[test]
    fn dismissing_an_exited_worker_answers_with_its_token_and_whether_a_row_was_there() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        let token = store.add_worker(brigade_id, None).unwrap();
        let store = RefCell::new(store);
        let intent = |token: &str, repeated| StoreIntent::DismissExitedWorker {
            brigade_id,
            token: token.to_string(),
            repeated,
        };

        let events = execute_store_intent(intent(&token, true), &store);
        let [
            Event::ExitedWorkerDismissed {
                brigade_id: id,
                token: answered,
                repeated,
                result: Ok((removed, ..)),
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(
            (*id, answered.as_str(), *repeated, *removed),
            (brigade_id, "worker-1", true, true)
        );
        assert!(
            store
                .borrow()
                .brigade_member(brigade_id, "worker-1")
                .unwrap()
                .is_none()
        );

        // Asked again for a row that is gone: removed = false.
        let events = execute_store_intent(intent("worker-1", false), &store);
        assert!(matches!(
            events.as_slice(),
            [Event::ExitedWorkerDismissed {
                result: Ok((false, ..)),
                ..
            }]
        ));
    }

    #[test]
    fn a_worker_that_ends_unspoken_is_gone_from_the_roster_brigade_status_reads() {
        // End to end over a temp file store: add a Worker, let its pane end
        // without it ever having spoken, run the removal the core asks for.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let mut writer = Store::open(&db).unwrap();
        let brigade_id = writer.create_brigade("cell").unwrap();
        writer
            .add_brigade_member(
                brigade_id,
                "director",
                BrigadeRole::Director,
                Some(&SessionId("dir".to_string())),
            )
            .unwrap();
        // The two Workers `staged_brigade_of_two_workers` stages.
        for _ in 0..2 {
            writer
                .add_worker(brigade_id, Some(AgentKind::ClaudeCode))
                .unwrap();
        }
        let store = RefCell::new(Store::open(&db).unwrap());
        let roster = |store: &RefCell<Store>| -> Vec<String> {
            store
                .borrow()
                .brigade_members(brigade_id)
                .unwrap()
                .into_iter()
                .map(|m| m.token)
                .collect()
        };
        assert_eq!(roster(&store), ["director", "worker-1", "worker-2"]);

        let (mut state, mut app, config) = staged_brigade_of_two_workers();
        let worker = SessionKey::from_id("new-worker::1::worker-1");
        // The pane ends on its own: there is no session row to resume.
        let mut events = VecDeque::from([Event::PtyExited {
            key: worker.clone(),
            reason: None,
        }]);
        while let Some(event) = events.pop_front() {
            for cmd in engine::update(&mut state, &mut app, &config, event, Instant::now()) {
                if let Cmd::Store(intent) = cmd {
                    events.extend(execute_store_intent(intent, &store));
                }
            }
        }

        // The roster brigade_status and the staging read no longer lists it ...
        assert_eq!(roster(&store), ["director", "worker-2"]);
        // ... the other Worker is untouched, the tile is gone, and the operator
        // was told.
        assert!(matches!(
            &state.stage,
            Stage::Brigade { panes, .. } if !panes.contains(&worker) && panes.len() == 2
        ));
        assert_eq!(
            state.displayed_status(),
            Some("worker-1 exited \u{2014} removed from the brigade")
        );
    }

    #[test]
    fn add_worker_reports_a_brigade_that_has_run_out_of_numbers() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(brigade_id, "worker-999999999", BrigadeRole::Worker, None)
            .unwrap();
        let store = RefCell::new(store);
        let err = add_worker_store(&store, brigade_id, AgentKind::ClaudeCode).unwrap_err();
        assert_eq!(err, "this brigade has run out of Worker numbers");
    }

    #[test]
    fn add_worker_after_dismissing_a_gap_mints_past_the_survivor_not_into_it() {
        // Correctness prerequisite: dismissing worker-1 while worker-2
        // survives must not make add_worker_store re-mint "worker-2" (a
        // count-based next_n would, colliding with the survivor and
        // handing it that member's stale store row/mail).
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(brigade_id, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        store
            .add_brigade_member(brigade_id, "worker-2", BrigadeRole::Worker, None)
            .unwrap();
        store.dismiss_worker(brigade_id, "worker-1").unwrap();
        let store = RefCell::new(store);

        let token = add_worker_store(&store, brigade_id, AgentKind::ClaudeCode).unwrap();

        assert_eq!(token, "worker-3");
        assert!(
            store
                .borrow()
                .brigade_member(brigade_id, "worker-2")
                .unwrap()
                .is_some(),
            "the survivor must be untouched"
        );
    }

    // --- id discovery: the handle map follows the core's rekey -----------

    /// A stand-in for the placeholder key the core mints for a Worker it
    /// has spawned but Claude hasn't named yet (`SessionKey::new_worker` is
    /// private to the core — only its shape matters here: not a real id).
    fn pending_key(token: &str) -> SessionKey {
        SessionKey::from_id(&format!("new-worker::1::{token}"))
    }

    // --- Cmd::CheckNewSessionCwd: is_dir() moved to the edge ----------------

    #[test]
    fn check_new_session_cwd_reports_whether_the_stat_finds_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let mut discovery = Vec::new();
        let meta_cache = MetaCache::new();
        let pty_host = MockPtyHost::default();
        let input_reader = MockInputReader::default();
        let thresholds = AgeThresholds::default();
        let brigade = BrigadeConfig::default();
        let claude_home = ClaudeHome::new(PathBuf::from("/nonexistent"));
        let agent_binaries = AgentBinaries::default();
        let enabled_agents = all_agents();
        let deps = Deps {
            claude_home: &claude_home,
            codex_home: None,
            thresholds: &thresholds,
            store: &store,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade: &brigade,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
            codex_catalog: &NoCatalog,
            child: None,
            exe: crate::exe::test_support::current(),
        };
        let mut handles = HashMap::new();

        let events = execute_cmd(
            Cmd::CheckNewSessionCwd {
                cwd: dir.path().to_path_buf(),
            },
            &deps,
            &mut handles,
            &mut discovery,
        );
        assert_eq!(
            events,
            vec![Event::NewSessionCwdChecked {
                cwd: dir.path().to_path_buf(),
                is_dir: true,
            }]
        );

        let missing = dir.path().join("does-not-exist");
        let events = execute_cmd(
            Cmd::CheckNewSessionCwd {
                cwd: missing.clone(),
            },
            &deps,
            &mut handles,
            &mut discovery,
        );
        assert_eq!(
            events,
            vec![Event::NewSessionCwdChecked {
                cwd: missing,
                is_dir: false,
            }]
        );
    }

    #[test]
    fn rekey_pty_moves_the_handle_from_the_synthetic_key_to_the_discovered_id() {
        let host = MockPtyHost::default();
        let mut handles = HashMap::new();
        let pending = pending_key("worker-1");
        let discovered = SessionKey::from_id("w1");
        handles.insert(pending.clone(), open(&host));
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let mut discovery = Vec::new();
        let meta_cache = MetaCache::new();
        let pty_host = MockPtyHost::default();
        let input_reader = MockInputReader::default();
        let thresholds = AgeThresholds::default();
        let brigade = BrigadeConfig::default();
        let claude_home = ClaudeHome::new(PathBuf::from("/nonexistent"));
        let agent_binaries = AgentBinaries::default();
        let enabled_agents = all_agents();
        let deps = Deps {
            claude_home: &claude_home,
            codex_home: None,
            thresholds: &thresholds,
            store: &store,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade: &brigade,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
            codex_catalog: &NoCatalog,
            child: None,
            exe: crate::exe::test_support::current(),
        };

        let events = execute_cmd(
            Cmd::RekeyPty {
                from: pending.clone(),
                to: discovered.clone(),
            },
            &deps,
            &mut handles,
            &mut discovery,
        );

        assert!(events.is_empty());
        assert!(!handles.contains_key(&pending));
        assert!(
            handles.contains_key(&discovered),
            "the same live child, now reachable under its real id"
        );
    }

    /// `execute_cmd` for one command against `handles`, with the dependencies a
    /// command that touches only the handles and the store needs.
    fn run_cmd(
        store: Store,
        cmd: Cmd,
        handles: &mut HashMap<SessionKey, PtyHandle>,
    ) -> (Vec<Event>, RefCell<Store>) {
        let store = RefCell::new(store);
        let mut discovery = Vec::new();
        let meta_cache = MetaCache::new();
        let pty_host = MockPtyHost::default();
        let input_reader = MockInputReader::default();
        let thresholds = AgeThresholds::default();
        let brigade = BrigadeConfig::default();
        let claude_home = ClaudeHome::new(PathBuf::from("/nonexistent"));
        let agent_binaries = AgentBinaries::default();
        let enabled_agents = all_agents();
        let events = {
            let deps = Deps {
                claude_home: &claude_home,
                codex_home: None,
                thresholds: &thresholds,
                store: &store,
                meta_cache: &meta_cache,
                pty_host: &pty_host,
                input_reader: &input_reader,
                brigade: &brigade,
                agent_binaries: &agent_binaries,
                enabled_agents: &enabled_agents,
                codex_catalog: &NoCatalog,
                child: None,
                exe: crate::exe::test_support::current(),
            };
            execute_cmd(cmd, &deps, handles, &mut discovery)
        };
        (events, store)
    }

    #[test]
    fn interrupt_pty_writes_one_esc_to_the_live_child_and_holds_the_next_write() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let host = MockPtyHost {
            captured: captured.clone(),
            ..Default::default()
        };
        let key = SessionKey::from_id("sess-1");
        let mut handles = HashMap::new();
        handles.insert(key.clone(), open(&host));

        let (events, _) = run_cmd(
            Store::open_in_memory().unwrap(),
            Cmd::InterruptPty { key: key.clone() },
            &mut handles,
        );
        assert!(events.is_empty());
        assert_eq!(&*captured.lock().unwrap(), b"\x1b");

        // The next write through the same command path is held, not merged.
        let (events, _) = run_cmd(
            Store::open_in_memory().unwrap(),
            Cmd::WritePty {
                key,
                bytes: b"hello".to_vec(),
            },
            &mut handles,
        );
        assert!(events.is_empty(), "held is not dropped");
        assert_eq!(&*captured.lock().unwrap(), b"\x1b", "still only the Esc");
    }

    #[test]
    fn what_an_interrupt_held_back_survives_a_rekey_and_flushes_in_order() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let host = MockPtyHost {
            captured: captured.clone(),
            ..Default::default()
        };
        let pending = pending_key("worker-1");
        let discovered = SessionKey::from_id("w1");
        let mut handles = HashMap::new();
        handles.insert(pending.clone(), open(&host));

        run_cmd(
            Store::open_in_memory().unwrap(),
            Cmd::InterruptPty {
                key: pending.clone(),
            },
            &mut handles,
        );
        for bytes in [b"one".as_slice(), b"two".as_slice()] {
            run_cmd(
                Store::open_in_memory().unwrap(),
                Cmd::WritePty {
                    key: pending.clone(),
                    bytes: bytes.to_vec(),
                },
                &mut handles,
            );
        }
        run_cmd(
            Store::open_in_memory().unwrap(),
            Cmd::RekeyPty {
                from: pending.clone(),
                to: discovered.clone(),
            },
            &mut handles,
        );
        assert!(!handles.contains_key(&pending));
        assert_eq!(&*captured.lock().unwrap(), b"\x1b", "still held");

        std::thread::sleep(
            crate::embedded::quiet_writes::QUIET_AFTER_INTERRUPT
                + std::time::Duration::from_millis(30),
        );
        handles.get_mut(&discovered).unwrap().flush_quiet();
        assert_eq!(
            &*captured.lock().unwrap(),
            b"\x1bonetwo",
            "the held bytes moved with the handle and left in order"
        );
    }

    #[test]
    fn what_an_interrupt_held_back_goes_with_the_handle_when_the_pane_ends() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let host = MockPtyHost {
            captured: captured.clone(),
            ..Default::default()
        };
        let key = SessionKey::from_id("sess-1");
        let mut handles = HashMap::new();
        handles.insert(key.clone(), open(&host));
        run_cmd(
            Store::open_in_memory().unwrap(),
            Cmd::InterruptPty { key: key.clone() },
            &mut handles,
        );
        run_cmd(
            Store::open_in_memory().unwrap(),
            Cmd::WritePty {
                key: key.clone(),
                bytes: b"never".to_vec(),
            },
            &mut handles,
        );
        // The pane ends by a kill and the loop drops the handle once it reports its
        // exit, as it does for a natural exit too.
        run_cmd(
            Store::open_in_memory().unwrap(),
            Cmd::KillPty { key: key.clone() },
            &mut handles,
        );
        drop(handles.remove(&key));
        std::thread::sleep(
            crate::embedded::quiet_writes::QUIET_AFTER_INTERRUPT
                + std::time::Duration::from_millis(30),
        );
        assert_eq!(
            &*captured.lock().unwrap(),
            b"\x1b",
            "nothing is written after the handle is gone"
        );
    }

    #[test]
    fn interrupt_pty_against_a_key_with_no_live_handle_reports_it() {
        let key = SessionKey::from_id("gone");
        let (events, _) = run_cmd(
            Store::open_in_memory().unwrap(),
            Cmd::InterruptPty { key: key.clone() },
            &mut HashMap::new(),
        );
        assert_eq!(events, vec![Event::PtyWriteDropped { key }]);
    }

    #[test]
    fn recording_an_interrupt_outcome_reaches_the_store_once() {
        use banto_core::model::{InterruptOutcome, InterruptWhy};
        use banto_io::store::InterruptState;
        let mut store = Store::open_in_memory().unwrap();
        store
            .add_brigade_member(1, "director", BrigadeRole::Director, None)
            .unwrap();
        store
            .add_brigade_member(1, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        let id = store
            .enqueue_interrupt_message(1, "director", "worker-1", "m")
            .unwrap()
            .unwrap();
        let record = |outcome, why| StoreIntent::RecordInterruptOutcome {
            brigade_id: 1,
            token: "worker-1".to_string(),
            message_id: id,
            outcome,
            why,
        };
        let store = RefCell::new(store);
        assert!(execute_store_intent(record(InterruptOutcome::Pressed, None), &store).is_empty());
        assert_eq!(
            store.borrow().interrupt_state(id).unwrap(),
            InterruptState::Decided(InterruptOutcome::Pressed)
        );
        // `why` is for the pane log; the store keeps the outcome alone, and a second
        // decision does not overwrite the first.
        execute_store_intent(
            record(InterruptOutcome::NotMidTurn, Some(InterruptWhy::Idle)),
            &store,
        );
        assert_eq!(
            store.borrow().interrupt_state(id).unwrap(),
            InterruptState::Decided(InterruptOutcome::Pressed)
        );
    }

    #[test]
    fn the_tick_is_given_each_undecided_interrupt_with_its_age_and_whether_it_was_read() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .add_brigade_member(1, "director", BrigadeRole::Director, None)
            .unwrap();
        store
            .add_brigade_member(1, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        assert!(interrupt_facts(&store, 1).unwrap().is_empty());
        let id = store
            .enqueue_interrupt_message(1, "director", "worker-1", "m")
            .unwrap()
            .unwrap();
        let facts = interrupt_facts(&store, 1).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].message_id, id);
        assert_eq!(facts[0].member, "worker-1");
        assert!(!facts[0].read);
        assert!(facts[0].age_ms < 5_000, "just written: {}", facts[0].age_ms);

        store
            .fetch_brigade_messages(1, "worker-1", BrigadeRole::Worker)
            .unwrap();
        assert!(
            interrupt_facts(&store, 1).unwrap()[0].read,
            "pulled before anyone looked"
        );

        store
            .record_interrupt_outcome(id, banto_core::model::InterruptOutcome::Read)
            .unwrap();
        assert!(
            interrupt_facts(&store, 1).unwrap().is_empty(),
            "decided: not listed again"
        );
        assert!(
            interrupt_facts(&store, 2).unwrap().is_empty(),
            "another brigade's is not this one's"
        );
    }

    #[test]
    fn write_pty_reaches_the_child_and_reports_nothing_when_the_handle_is_live() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let host = MockPtyHost {
            captured: captured.clone(),
            ..Default::default()
        };
        let key = SessionKey::from_id("sess-1");
        let mut handles = HashMap::new();
        handles.insert(key.clone(), open(&host));
        let mut discovery = Vec::new();
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let meta_cache = MetaCache::new();
        let pty_host = MockPtyHost::default();
        let input_reader = MockInputReader::default();
        let thresholds = AgeThresholds::default();
        let brigade = BrigadeConfig::default();
        let claude_home = ClaudeHome::new(PathBuf::from("/nonexistent"));
        let agent_binaries = AgentBinaries::default();
        let enabled_agents = all_agents();
        let deps = Deps {
            claude_home: &claude_home,
            codex_home: None,
            thresholds: &thresholds,
            store: &store,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade: &brigade,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
            codex_catalog: &NoCatalog,
            child: None,
            exe: crate::exe::test_support::current(),
        };

        let events = execute_cmd(
            Cmd::WritePty {
                key,
                bytes: b"hello".to_vec(),
            },
            &deps,
            &mut handles,
            &mut discovery,
        );

        assert!(events.is_empty());
        assert_eq!(&*captured.lock().unwrap(), b"hello");
    }

    #[test]
    fn write_pty_against_a_key_with_no_live_handle_reports_it_instead_of_vanishing() {
        // The regression this pins: before `Event::PtyWriteDropped` existed,
        // this branch returned `Vec::new()` — a `Cmd::WritePty` whose
        // target had already been renamed or closed (the exact shape of
        // the Goinkyo kickoff bug `pending_goinkyo_kickoffs`'s own rename
        // fix addresses) simply vanished, with nothing left to say a write
        // was ever attempted.
        let key = SessionKey::from_id("gone");
        let mut handles = HashMap::new();
        let mut discovery = Vec::new();
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let meta_cache = MetaCache::new();
        let pty_host = MockPtyHost::default();
        let input_reader = MockInputReader::default();
        let thresholds = AgeThresholds::default();
        let brigade = BrigadeConfig::default();
        let claude_home = ClaudeHome::new(PathBuf::from("/nonexistent"));
        let agent_binaries = AgentBinaries::default();
        let enabled_agents = all_agents();
        let deps = Deps {
            claude_home: &claude_home,
            codex_home: None,
            thresholds: &thresholds,
            store: &store,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade: &brigade,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
            codex_catalog: &NoCatalog,
            child: None,
            exe: crate::exe::test_support::current(),
        };

        let events = execute_cmd(
            Cmd::WritePty {
                key: key.clone(),
                bytes: b"hello".to_vec(),
            },
            &deps,
            &mut handles,
            &mut discovery,
        );

        assert_eq!(events, vec![Event::PtyWriteDropped { key }]);
    }

    /// Write a session jsonl whose head records `cwd`, the shape
    /// `find_new_sessions` matches on.
    fn write_session_at(claude_home: &Path, id: &str, cwd: &Path) {
        let dir = claude_home.join("projects").join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!(
                "{{\"type\":\"mode\",\"cwd\":{}}}\n",
                serde_json::to_string(&cwd.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();
    }

    /// Write a `sessions/<pid>.json` live-state file, the thing `claude`
    /// publishes at startup — before any session history exists.
    fn write_live_state(claude_home: &Path, pid: u32, session_id: &str, cwd: &Path) {
        write_live_state_with_status(claude_home, pid, session_id, cwd, "idle");
    }

    /// Same as [`write_live_state`], but with an explicit `status` rather
    /// than always `"idle"` — for exercising the `"busy"`/`"waiting"` arms
    /// of [`gather_relay_observations`]'s Claude-side idle check.
    fn write_live_state_with_status(
        claude_home: &Path,
        pid: u32,
        session_id: &str,
        cwd: &Path,
        status: &str,
    ) {
        let dir = claude_home.join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{pid}.json")),
            format!(
                "{{\"pid\":{pid},\"sessionId\":\"{session_id}\",\"cwd\":{},\"status\":{}}}",
                serde_json::to_string(&cwd.to_string_lossy()).unwrap(),
                serde_json::to_string(status).unwrap()
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_worker_that_has_written_no_session_file_is_still_identified_by_its_pid() {
        // See poll_discovery's doc for the deadlock this covers.
        let claude_home = tempfile::tempdir().unwrap();
        let cwd = PathBuf::from("/work/alpha");
        let provider = ClaudeCodeProvider::new(ClaudeHome::new(claude_home.path().to_path_buf()));
        write_live_state(claude_home.path(), 4242, "w1", &cwd);
        let live = read_live_sessions(&claude_home.path().join("sessions"));

        let mut trackers = vec![DiscoveryTracker {
            key: pending_key("worker-1"),
            agent: AgentKind::ClaudeCode,
            spawned: DirectorySpellings::of(&cwd),
            cwd: cwd.clone(),
            since: SystemTime::now(),
            member: Some((1, "worker-1".to_string())),
            pid: Some(4242),
            codex_log_scan: None,
            notified_untrusted: false,
        }];
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let claude_home = ClaudeHome::new(claude_home.path().to_path_buf());

        let events = poll_discovery(
            &mut trackers,
            &provider,
            &HashSet::new(),
            &live,
            &store,
            &claude_home,
        );

        assert!(
            matches!(
                events.as_slice(),
                [Event::DiscoveryResult { session_id, member, .. }]
                    if session_id == "w1" && member.as_ref().unwrap().1 == "worker-1"
            ),
            "no jsonl exists at all; the live-state file is the only source: {events:?}"
        );
        assert!(trackers.is_empty());
    }

    #[test]
    fn a_live_state_file_for_a_recycled_pid_in_another_cwd_is_declined() {
        // Stale `sessions/<pid>.json` whose pid the OS handed to our child:
        // adopting it would hand this Worker an unrelated session that banto
        // would later `--resume` a second time. The cwd it records is what
        // gives it away.
        let claude_home = tempfile::tempdir().unwrap();
        let provider = ClaudeCodeProvider::new(ClaudeHome::new(claude_home.path().to_path_buf()));
        write_live_state(
            claude_home.path(),
            4242,
            "someone-elses-session",
            Path::new("/work/beta"),
        );
        let live = read_live_sessions(&claude_home.path().join("sessions"));

        let mut trackers = vec![DiscoveryTracker {
            key: pending_key("worker-1"),
            agent: AgentKind::ClaudeCode,
            spawned: DirectorySpellings::of(Path::new("/work/alpha")),
            cwd: PathBuf::from("/work/alpha"),
            since: SystemTime::now(),
            member: Some((1, "worker-1".to_string())),
            pid: Some(4242),
            codex_log_scan: None,
            notified_untrusted: false,
        }];
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let claude_home = ClaudeHome::new(claude_home.path().to_path_buf());

        assert!(
            poll_discovery(
                &mut trackers,
                &provider,
                &HashSet::new(),
                &live,
                &store,
                &claude_home
            )
            .is_empty(),
            "a different cwd means a different session"
        );
        assert_eq!(trackers.len(), 1, "still pending, not resolved wrongly");
    }

    #[test]
    fn two_workers_in_one_cwd_never_resolve_to_the_same_id_across_passes() {
        // The dogfooding bug: two Workers auto-spawned into the Director's
        // cwd. `used_this_pass` only separates them when both jsonl files
        // already exist in the same pass; when the second appears a pass
        // later, the *claimed* set is the only thing keeping the second
        // tracker off the first one's id — and it is read from the handle
        // map, so it is only correct because `Cmd::RekeyPty` renamed that
        // handle. Before the fix both Workers resolved to one id: two tiles
        // titled "worker 1", one live child, and two membership rows
        // claiming the same session.
        let claude_home = tempfile::tempdir().unwrap();
        let cwd = PathBuf::from("/work/alpha");
        let provider = ClaudeCodeProvider::new(ClaudeHome::new(claude_home.path().to_path_buf()));
        let since = SystemTime::now() - Duration::from_secs(1);
        // `pid: None` is the fallback shape this test is about: no usable
        // child pid, so only the session-file scan can answer.
        let tracker = |token: &str| DiscoveryTracker {
            key: pending_key(token),
            agent: AgentKind::ClaudeCode,
            spawned: DirectorySpellings::of(&cwd),
            cwd: cwd.clone(),
            since,
            member: Some((1, token.to_string())),
            pid: None,
            codex_log_scan: None,
            notified_untrusted: false,
        };
        let mut trackers = vec![tracker("worker-1"), tracker("worker-2")];
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let claude_home_ref = ClaudeHome::new(claude_home.path().to_path_buf());

        // Pass 1: only the first Worker's session file exists yet.
        write_session_at(claude_home.path(), "w1", &cwd);
        let mut claimed: HashSet<String> = trackers
            .iter()
            .map(|tracker| tracker.key.as_str().to_string())
            .collect();
        let events = poll_discovery(
            &mut trackers,
            &provider,
            &claimed,
            &[],
            &store,
            &claude_home_ref,
        );
        assert!(matches!(
            events.as_slice(),
            [Event::DiscoveryResult { session_id, .. }] if session_id == "w1"
        ));
        assert_eq!(trackers.len(), 1, "only worker-2 is still pending");

        // `Cmd::RekeyPty` has since renamed the resolved pane's handle, so
        // the next pass sees "w1" as taken.
        claimed.remove(pending_key("worker-1").as_str());
        claimed.insert("w1".to_string());

        // Pass 2: the second Worker's file appears.
        write_session_at(claude_home.path(), "w2", &cwd);
        let events = poll_discovery(
            &mut trackers,
            &provider,
            &claimed,
            &[],
            &store,
            &claude_home_ref,
        );
        assert!(
            matches!(
                events.as_slice(),
                [Event::DiscoveryResult { session_id, .. }] if session_id == "w2"
            ),
            "worker-2 must take the unclaimed id, not re-take worker-1's: {events:?}"
        );
        assert!(trackers.is_empty());
    }

    // --- Codex Worker discovery: the store-based source, and waiting for it --

    fn codex_tracker(brigade_id: BrigadeId, token: &str, since: SystemTime) -> DiscoveryTracker {
        DiscoveryTracker {
            key: pending_key(token),
            agent: AgentKind::Codex,
            spawned: DirectorySpellings::of(Path::new("/work/alpha")),
            cwd: PathBuf::from("/work/alpha"),
            since,
            member: Some((brigade_id, token.to_string())),
            pid: None,
            codex_log_scan: None,
            notified_untrusted: false,
        }
    }

    #[test]
    fn codex_discovery_does_not_claim_a_claude_session_in_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(dir.path().to_path_buf());
        let provider = ClaudeCodeProvider::new(home.clone());
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let since = SystemTime::now() - Duration::from_secs(2);
        write_session_at(dir.path(), "unrelated-claude", Path::new("/work/alpha"));
        let mut trackers = vec![codex_tracker(1, "worker-1", since)];
        assert!(
            poll_discovery(
                &mut trackers,
                &provider,
                &HashSet::new(),
                &[],
                &store,
                &home
            )
            .is_empty()
        );
        assert_eq!(trackers.len(), 1);
    }

    #[test]
    fn solo_codex_discovery_keeps_waiting_however_long() {
        let dir = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(dir.path().to_path_buf());
        let provider = ClaudeCodeProvider::new(home.clone());
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let mut tracker = codex_tracker(1, "solo", SystemTime::now() - Duration::from_secs(3600));
        tracker.member = None;
        let mut trackers = vec![tracker];
        assert!(
            poll_discovery(
                &mut trackers,
                &provider,
                &HashSet::new(),
                &[],
                &store,
                &home
            )
            .is_empty()
        );
        assert_eq!(trackers.len(), 1);
    }

    #[test]
    fn unresolved_codex_tracker_blocks_resuming_a_known_codex_at_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = SpawnFixture::new(dir.path().to_path_buf());
        let mut handles = HashMap::new();
        let mut tracker = codex_tracker(1, "solo", SystemTime::now());
        tracker.member = None;
        let mut discovery = vec![tracker];
        let events = execute_cmd(
            open_embedded_cmd(AgentKind::Codex, "known", dir.path()),
            &fixture.deps(),
            &mut handles,
            &mut discovery,
        );
        assert!(matches!(events.as_slice(), [Event::SpawnFailed { .. }]));
        assert!(fixture.launches().is_empty());
    }

    #[test]
    fn codex_discovery_does_not_use_a_claude_live_pid_match() {
        let dir = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(dir.path().to_path_buf());
        let provider = ClaudeCodeProvider::new(home.clone());
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let cwd = PathBuf::from("/work/alpha");
        write_live_state(dir.path(), 4242, "unrelated-claude", &cwd);
        let live = read_live_sessions(&home.sessions_dir());
        let mut tracker = codex_tracker(1, "worker-1", SystemTime::now());
        tracker.pid = Some(4242);
        let mut trackers = vec![tracker];
        assert!(
            poll_discovery(
                &mut trackers,
                &provider,
                &HashSet::new(),
                &live,
                &store,
                &home
            )
            .is_empty()
        );
        assert_eq!(trackers.len(), 1);
    }

    #[test]
    fn solo_codex_discovery_correlates_only_the_hosted_child() {
        struct Starts;
        impl banto_io::codex_liveness::ProcessStartTime for Starts {
            fn start_time(&self, pid: u32) -> Option<u64> {
                (pid == 100).then_some(900)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let home = CodexHome::new(dir.path().to_path_buf());
        let conn = rusqlite::Connection::open(home.threads_db_path()).unwrap();
        conn.execute_batch("CREATE TABLE threads (id TEXT, cwd TEXT, updated_at_ms INTEGER, source TEXT); INSERT INTO threads VALUES ('own', '/work/alpha', 1000000, 'cli'), ('other', '/work/alpha', 1000000, 'cli');").unwrap();
        let conn = rusqlite::Connection::open(home.logs_db_path()).unwrap();
        conn.execute_batch("CREATE TABLE logs (id INTEGER PRIMARY KEY, ts INTEGER, ts_nanos INTEGER, thread_id TEXT, process_uuid TEXT);").unwrap();
        let mut scan = banto_io::codex_liveness::CodexLogScan::open(&home).unwrap();
        assert!(scan.bind_pid(100, &Starts));
        conn.execute_batch("INSERT INTO logs VALUES (1, 1000, 0, 'own', 'pid:100:abc'), (2, 1000, 0, 'other', 'pid:101:def');").unwrap();
        let mut tracker =
            codex_tracker(1, "solo", SystemTime::UNIX_EPOCH + Duration::from_secs(900));
        tracker.member = None;
        tracker.pid = Some(100);
        tracker.codex_log_scan = Some(scan);
        let mut trackers = vec![tracker];
        let events = poll_solo_codex_discovery(&mut trackers, &home, &HashSet::new(), &Starts);
        assert!(
            matches!(events.as_slice(), [Event::DiscoveryResult { session_id, member: None, .. }] if session_id == "own")
        );
        assert!(trackers.is_empty());
    }

    #[test]
    fn a_codex_worker_resolves_via_the_stores_briefed_session_id() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(brigade_id, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        store
            .record_briefing(
                brigade_id,
                "worker-1",
                &SessionId("w1".to_string()),
                SystemTime::now(),
            )
            .unwrap();
        let store = RefCell::new(store);
        let claude_home = tempfile::tempdir().unwrap();
        let provider = ClaudeCodeProvider::new(ClaudeHome::new(claude_home.path().to_path_buf()));
        let claude_home_ref = ClaudeHome::new(claude_home.path().to_path_buf());

        // Spawned before the hook recorded the briefing above.
        let spawned = SystemTime::now() - Duration::from_secs(5);
        let mut trackers = vec![codex_tracker(brigade_id, "worker-1", spawned)];

        let events = poll_discovery(
            &mut trackers,
            &provider,
            &HashSet::new(),
            &[],
            &store,
            &claude_home_ref,
        );

        assert!(
            matches!(
                events.as_slice(),
                [Event::DiscoveryResult { session_id, member, .. }]
                    if session_id == "w1" && member.as_ref().unwrap().1 == "worker-1"
            ),
            "no live-state file or session file exists for Codex; the store is the only source: {events:?}"
        );
        assert!(trackers.is_empty());
    }

    #[test]
    fn a_briefing_left_over_from_an_earlier_process_does_not_identify_a_fresh_pane() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(brigade_id, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        let spawned = SystemTime::now();
        store
            .record_briefing(
                brigade_id,
                "worker-1",
                &SessionId("old".to_string()),
                spawned - Duration::from_secs(60),
            )
            .unwrap();
        let store = RefCell::new(store);
        let claude_home = tempfile::tempdir().unwrap();
        let provider = ClaudeCodeProvider::new(ClaudeHome::new(claude_home.path().to_path_buf()));
        let claude_home_ref = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut trackers = vec![codex_tracker(brigade_id, "worker-1", spawned)];

        for _ in 0..2 {
            assert!(
                poll_discovery(
                    &mut trackers,
                    &provider,
                    &HashSet::new(),
                    &[],
                    &store,
                    &claude_home_ref,
                )
                .is_empty(),
                "the old process's briefing is not this pane's session"
            );
            assert_eq!(trackers.len(), 1, "still waiting for the new process");
        }

        // The new process takes its first turn and its hook records its own id.
        store
            .borrow_mut()
            .record_briefing(
                brigade_id,
                "worker-1",
                &SessionId("new".to_string()),
                spawned + Duration::from_secs(5),
            )
            .unwrap();
        let events = poll_discovery(
            &mut trackers,
            &provider,
            &HashSet::new(),
            &[],
            &store,
            &claude_home_ref,
        );
        assert!(
            matches!(
                events.as_slice(),
                [Event::DiscoveryResult { session_id, .. }] if session_id == "new"
            ),
            "a briefing written after the spawn identifies the pane: {events:?}"
        );
        assert!(trackers.is_empty());
    }

    #[test]
    fn a_claude_tracker_never_consults_the_stores_briefed_session_id() {
        // Defends the `agent == AgentKind::Codex` gate itself: even with a
        // matching (and misleading, in practice never-set-for-Claude)
        // briefed_session_id sitting in the store, a Claude tracker must
        // never resolve from it.
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(brigade_id, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        store
            .record_briefing(
                brigade_id,
                "worker-1",
                &SessionId("w1".to_string()),
                SystemTime::now(),
            )
            .unwrap();
        let store = RefCell::new(store);
        let claude_home = tempfile::tempdir().unwrap();
        let provider = ClaudeCodeProvider::new(ClaudeHome::new(claude_home.path().to_path_buf()));
        let claude_home_ref = ClaudeHome::new(claude_home.path().to_path_buf());

        let mut trackers = vec![DiscoveryTracker {
            key: pending_key("worker-1"),
            agent: AgentKind::ClaudeCode,
            spawned: DirectorySpellings::of(Path::new("/work/alpha")),
            cwd: PathBuf::from("/work/alpha"),
            since: SystemTime::now(),
            member: Some((brigade_id, "worker-1".to_string())),
            pid: None,
            codex_log_scan: None,
            notified_untrusted: false,
        }];

        assert!(
            poll_discovery(
                &mut trackers,
                &provider,
                &HashSet::new(),
                &[],
                &store,
                &claude_home_ref
            )
            .is_empty()
        );
        assert_eq!(
            trackers.len(),
            1,
            "still pending — no Claude source resolved it"
        );
    }

    #[test]
    fn a_codex_worker_is_identified_however_long_after_spawn() {
        // The hook records the session id when the first turn happens — which may
        // be long after spawn (the kickoff held behind a trust prompt, or the
        // operator typing the first turn themselves) — so no amount of waiting
        // without it drops the tracker.
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(brigade_id, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        let store = RefCell::new(store);
        let claude_home = tempfile::tempdir().unwrap();
        let provider = ClaudeCodeProvider::new(ClaudeHome::new(claude_home.path().to_path_buf()));
        let claude_home_ref = ClaudeHome::new(claude_home.path().to_path_buf());
        let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
        let mut trackers = vec![codex_tracker(brigade_id, "worker-1", an_hour_ago)];

        for _ in 0..3 {
            assert!(
                poll_discovery(
                    &mut trackers,
                    &provider,
                    &HashSet::new(),
                    &[],
                    &store,
                    &claude_home_ref,
                )
                .is_empty()
            );
            assert_eq!(trackers.len(), 1, "still waiting, nothing reported");
        }

        store
            .borrow_mut()
            .record_briefing(
                brigade_id,
                "worker-1",
                &SessionId("w1".to_string()),
                SystemTime::now(),
            )
            .unwrap();
        let events = poll_discovery(
            &mut trackers,
            &provider,
            &HashSet::new(),
            &[],
            &store,
            &claude_home_ref,
        );

        assert!(
            matches!(
                events.as_slice(),
                [Event::DiscoveryResult { session_id, member, .. }]
                    if session_id == "w1" && member.as_ref().unwrap().1 == "worker-1"
            ),
            "{events:?}"
        );
        assert!(trackers.is_empty());
    }

    #[test]
    fn a_claude_worker_keeps_waiting_however_long() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let claude_home = tempfile::tempdir().unwrap();
        let provider = ClaudeCodeProvider::new(ClaudeHome::new(claude_home.path().to_path_buf()));
        let claude_home_ref = ClaudeHome::new(claude_home.path().to_path_buf());
        let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
        let mut trackers = vec![DiscoveryTracker {
            key: pending_key("worker-1"),
            agent: AgentKind::ClaudeCode,
            spawned: DirectorySpellings::of(Path::new("/work/alpha")),
            cwd: PathBuf::from("/work/alpha"),
            since: an_hour_ago,
            member: Some((1, "worker-1".to_string())),
            pid: None,
            codex_log_scan: None,
            notified_untrusted: false,
        }];

        assert!(
            poll_discovery(
                &mut trackers,
                &provider,
                &HashSet::new(),
                &[],
                &store,
                &claude_home_ref
            )
            .is_empty()
        );
        assert_eq!(trackers.len(), 1, "still pending");
    }

    #[test]
    fn a_pane_that_ends_unidentified_takes_its_tracker_with_it() {
        // The tracker has no clock to end it, so the pane's screen going away is
        // its end — the same `PtyExited` that releases the core's own guard.
        let key = pending_key("worker-1");
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = App::new(vec![]);
        state.screens.insert(key.clone(), Screen::new(24, 80));
        let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
        let mut trackers = vec![codex_tracker(1, "worker-1", an_hour_ago)];

        drop_trackers_of_gone_panes(&mut trackers, &state);
        assert_eq!(trackers.len(), 1, "the pane is still there");

        engine::update(
            &mut state,
            &mut app,
            &BrigadeConfig::default(),
            Event::PtyExited { key, reason: None },
            Instant::now(),
        );
        drop_trackers_of_gone_panes(&mut trackers, &state);

        assert!(trackers.is_empty());
    }

    /// Root a [`ClaudeHome`] under `dir` with a `.claude.json` registry
    /// beside it (`trust_registry_path`'s own layout), matching
    /// `directory_trust`'s own test fixture.
    fn claude_home_with_trust_registry(dir: &tempfile::TempDir, registry_text: &str) -> ClaudeHome {
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join(".claude.json"), registry_text).unwrap();
        ClaudeHome::new(dir.path().join(".claude"))
    }

    #[test]
    fn a_claude_worker_whose_cwd_reads_not_trusted_reports_it_once() {
        // The deadlock poll_discovery's own doc covers: sitting behind an
        // unanswered trust prompt, `claude` never writes a session file, so
        // this is the only way it's ever explained rather than just silent.
        let dir = tempfile::tempdir().unwrap();
        let claude_home = claude_home_with_trust_registry(
            &dir,
            r#"{"projects": {"/work/alpha": {"hasTrustDialogAccepted": false}}}"#,
        );
        let provider = ClaudeCodeProvider::new(claude_home.clone());
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let mut trackers = vec![DiscoveryTracker {
            key: pending_key("worker-1"),
            agent: AgentKind::ClaudeCode,
            spawned: DirectorySpellings::of(Path::new("/work/alpha")),
            cwd: PathBuf::from("/work/alpha"),
            since: SystemTime::now(),
            member: Some((1, "worker-1".to_string())),
            pid: None,
            codex_log_scan: None,
            notified_untrusted: false,
        }];

        let events = poll_discovery(
            &mut trackers,
            &provider,
            &HashSet::new(),
            &[],
            &store,
            &claude_home,
        );
        assert!(
            matches!(
                events.as_slice(),
                [Event::ClaudeWorkerDirectoryUntrusted { token }] if token == "worker-1"
            ),
            "{events:?}"
        );
        assert_eq!(
            trackers.len(),
            1,
            "still pending — discovery keeps retrying"
        );

        // A second poll under the same NotTrusted registry stays silent —
        // the notice is one-shot, not repeated every tick.
        let events = poll_discovery(
            &mut trackers,
            &provider,
            &HashSet::new(),
            &[],
            &store,
            &claude_home,
        );
        assert!(events.is_empty(), "already notified once: {events:?}");
    }

    #[test]
    fn a_claude_worker_whose_cwd_has_no_trust_record_never_reports_untrusted() {
        // The narrowing this gate deliberately keeps over Codex's own
        // (`NotTrusted` only, not `Unknown` too — see poll_discovery's doc):
        // a directory nobody has ever opened Claude Code into reads as
        // `Unknown`, not `NotTrusted`, and must not be mistaken for a stuck
        // prompt.
        let dir = tempfile::tempdir().unwrap();
        let claude_home = ClaudeHome::new(dir.path().join(".claude"));
        let provider = ClaudeCodeProvider::new(claude_home.clone());
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let mut trackers = vec![DiscoveryTracker {
            key: pending_key("worker-1"),
            agent: AgentKind::ClaudeCode,
            spawned: DirectorySpellings::of(Path::new("/work/alpha")),
            cwd: PathBuf::from("/work/alpha"),
            since: SystemTime::now(),
            member: Some((1, "worker-1".to_string())),
            pid: None,
            codex_log_scan: None,
            notified_untrusted: false,
        }];

        assert!(
            poll_discovery(
                &mut trackers,
                &provider,
                &HashSet::new(),
                &[],
                &store,
                &claude_home
            )
            .is_empty()
        );
        assert_eq!(trackers.len(), 1);
    }

    #[test]
    fn a_codex_worker_in_a_not_trusted_cwd_never_reports_the_claude_specific_event() {
        // Defends the `agent == AgentKind::ClaudeCode` gate itself: Claude's
        // registry says nothing about a Codex pane, and this notice exists
        // only to explain Claude's silence.
        let dir = tempfile::tempdir().unwrap();
        let claude_home = claude_home_with_trust_registry(
            &dir,
            r#"{"projects": {"/work/alpha": {"hasTrustDialogAccepted": false}}}"#,
        );
        let provider = ClaudeCodeProvider::new(claude_home.clone());
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let mut trackers = vec![codex_tracker(1, "worker-1", SystemTime::now())];

        assert!(
            poll_discovery(
                &mut trackers,
                &provider,
                &HashSet::new(),
                &[],
                &store,
                &claude_home
            )
            .is_empty()
        );
        assert_eq!(
            trackers.len(),
            1,
            "still pending — this notice is Claude's alone"
        );
    }

    // --- session-move tracking: gather_session_moves ------------------

    #[test]
    fn gather_session_moves_detects_a_moved_worker_by_exact_pid_match() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1-old", Some(AgentKind::ClaudeCode));
        write_live_state(claude_home.path(), 4242, "w1-new", Path::new("/work/alpha"));

        let mut handles = HashMap::new();
        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        handles.insert(SessionKey::from_id("w1-old"), open(&host));

        let events = gather_session_moves_reading(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            &handles,
            &app,
            None,
        );

        assert!(
            matches!(
                events.as_slice(),
                [Event::MemberSessionForked { token, old_id, new_id, .. }]
                    if token == "worker-1" && old_id == "w1-old" && new_id == "w1-new"
            ),
            "expected exactly one move observation: {events:?}"
        );
    }

    #[test]
    fn gather_session_moves_detects_an_offstage_members_session_move() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, mut state, _app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        let offstage_brigade = store.borrow_mut().create_brigade("offstage").unwrap();
        store
            .borrow_mut()
            .add_brigade_member(
                offstage_brigade,
                "worker-1",
                BrigadeRole::Worker,
                Some(&SessionId("w2-old".to_string())),
            )
            .unwrap();
        let old_key = SessionKey::from_id("w2-old");
        state.screens.insert(old_key.clone(), Screen::new(24, 80));
        let app = App::new(vec![
            test_row("dir", AgentKind::ClaudeCode),
            test_row("w1", AgentKind::ClaudeCode),
            test_row("w2-old", AgentKind::ClaudeCode),
        ]);
        write_live_state(claude_home.path(), 4242, "w2-new", Path::new("/work/alpha"));

        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        let mut handles = HashMap::new();
        handles.insert(old_key, open(&host));

        let events = gather_session_moves_reading(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            &handles,
            &app,
            None,
        );

        assert!(matches!(
            events.as_slice(),
            [Event::MemberSessionForked {
                brigade_id,
                token,
                old_id,
                new_id,
                held_by_row: false,
            }] if *brigade_id == offstage_brigade
                && token == "worker-1"
                && old_id == "w2-old"
                && new_id == "w2-new"
        ));
    }

    #[test]
    fn gather_session_moves_is_silent_when_the_live_entry_still_reports_the_recorded_id() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        write_live_state(claude_home.path(), 4242, "w1", Path::new("/work/alpha"));

        let mut handles = HashMap::new();
        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        handles.insert(SessionKey::from_id("w1"), open(&host));

        let events = gather_session_moves_reading(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            &handles,
            &app,
            None,
        );

        assert!(events.is_empty(), "no move happened: {events:?}");
    }

    #[test]
    fn gather_session_moves_never_enters_the_ancestry_path_for_a_codex_member() {
        // Same exact-pid-match setup as
        // `gather_session_moves_detects_a_moved_worker_by_exact_pid_match`
        // — `entry.pid == pid` would trip `ancestry_reaches` at depth 0
        // with no real OS lookup at all, so if a Codex member were ever
        // read through Claude's live file, this would reliably produce a
        // (wrong) move observation instead of quietly doing nothing. The
        // only difference from the Claude test is the agent tag
        // `staged_worker` puts in `App`'s row. A Codex member's move has
        // its own source (the briefing its hook recorded, below).
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1-old", Some(AgentKind::Codex));
        write_live_state(claude_home.path(), 4242, "w1-new", Path::new("/work/alpha"));

        let mut handles = HashMap::new();
        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        handles.insert(SessionKey::from_id("w1-old"), open(&host));

        let events = gather_session_moves_reading(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            &handles,
            &app,
            None,
        );

        assert!(
            events.is_empty(),
            "a Codex member is never followed through Claude's live file: {events:?}"
        );
    }

    // --- session-move tracking: a Codex member that switched thread in place --

    const CODEX_PID: u32 = 4242;

    /// The per-tick events of `ticks` consecutive ticks of the move gather, one
    /// watch throughout, with `owns` standing in for Codex's log database.
    fn gather_move_ticks(
        state: &EmporiumState,
        store: &RefCell<Store>,
        handles: &HashMap<SessionKey, PtyHandle>,
        app: &App,
        owns: &dyn Fn(&str, u32) -> bool,
        ticks: usize,
    ) -> Vec<Vec<Event>> {
        let dir = tempfile::tempdir().unwrap();
        let claude_home = ClaudeHome::new(dir.path().to_path_buf());
        let mut watch = SessionMoveWatch::default();
        let view = ProcessView {
            probe: &SysinfoProbe,
            alive: &|_| true,
        };
        (0..ticks)
            .map(|_| {
                let read = TickRead::read(store, &claude_home, None);
                gather_session_moves(
                    &read,
                    state,
                    handles,
                    app,
                    &mut watch,
                    &MoveSources {
                        processes: &view,
                        owns_thread: owns,
                    },
                    None,
                )
            })
            .collect()
    }

    /// Store, staged state, session list, hosted panes, and the member's
    /// `(old, new)` session ids.
    type CodexMemberRig = (
        RefCell<Store>,
        EmporiumState,
        App,
        HashMap<SessionKey, PtyHandle>,
        (&'static str, &'static str),
    );

    /// A staged brigade whose Codex member (the Director, or Worker `worker-1`)
    /// holds thread `old` in a pane its process runs, and whose hook recorded the
    /// session id `new` `briefed_secs` seconds after the pane's launch (negative:
    /// before it; `None`: never). `new` is a listed Codex session when `listed`.
    /// Returns the member's `(old, new)` ids.
    fn codex_member_that_ran_its_hook_under_another_thread(
        director: bool,
        listed: bool,
        briefed_secs: Option<i64>,
    ) -> CodexMemberRig {
        codex_member_in_brigade_named("cell", director, listed, briefed_secs)
    }

    fn codex_member_in_brigade_named(
        brigade_name: &str,
        director: bool,
        listed: bool,
        briefed_secs: Option<i64>,
    ) -> CodexMemberRig {
        let (store, state, _) = staged_worker_in("w1-old", Some(AgentKind::Codex), brigade_name);
        let (token, old, new) = if director {
            ("director", "dir", "dir-new")
        } else {
            ("worker-1", "w1-old", "w1-new")
        };
        let agent_of = |id: &str| {
            if id == old {
                AgentKind::Codex
            } else {
                AgentKind::ClaudeCode
            }
        };
        let mut rows = vec![
            test_row("dir", agent_of("dir")),
            test_row("w1-old", agent_of("w1-old")),
        ];
        if listed {
            rows.push(test_row(new, AgentKind::Codex));
        }
        let app = App::new(rows);
        let host = MockPtyHost {
            pid: Some(CODEX_PID),
            ..Default::default()
        };
        let mut handles = HashMap::new();
        handles.insert(SessionKey::from_id(old), open(&host));
        if let Some(secs) = briefed_secs {
            let at = SystemTime::now();
            let at = if secs >= 0 {
                at + Duration::from_secs(secs as u64)
            } else {
                at - Duration::from_secs(secs.unsigned_abs())
            };
            let brigade_id = store.borrow().list_brigades().unwrap()[0].id;
            store
                .borrow_mut()
                .record_briefing(brigade_id, token, &SessionId(new.to_string()), at)
                .unwrap();
        }
        (store, state, app, handles, (old, new))
    }

    fn the_log_ties(thread: &'static str) -> impl Fn(&str, u32) -> bool {
        move |asked, pid| asked == thread && pid == CODEX_PID
    }

    fn forked_events(events: &[Event]) -> Vec<&Event> {
        events
            .iter()
            .filter(|event| matches!(event, Event::MemberSessionForked { .. }))
            .collect()
    }

    fn notices_in(per_tick: &[Vec<Event>]) -> Vec<String> {
        per_tick
            .iter()
            .flatten()
            .filter_map(|event| match event {
                Event::StartupNotices { notices } => Some(notices.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn apply(state: &mut EmporiumState, app: &mut App, event: Event) -> Vec<Cmd> {
        engine::update(state, app, &BrigadeConfig::default(), event, Instant::now())
    }

    #[test]
    fn a_codex_workers_new_thread_is_followed_by_its_row_and_its_pane() {
        let (store, mut state, mut app, handles, (old, new)) =
            codex_member_that_ran_its_hook_under_another_thread(false, true, Some(5));

        let ticks = gather_move_ticks(&state, &store, &handles, &app, &the_log_ties(new), 3);

        assert!(
            ticks[0].is_empty(),
            "believed only when it holds a second tick"
        );
        let [
            Event::MemberSessionForked {
                token,
                old_id,
                new_id,
                held_by_row: false,
                ..
            },
        ] = ticks[1].as_slice()
        else {
            panic!("expected exactly one move: {:?}", ticks[1]);
        };
        assert_eq!(
            (token.as_str(), old_id.as_str(), new_id.as_str()),
            ("worker-1", old, new)
        );
        assert!(notices_in(&ticks).is_empty());

        let cmds = apply(&mut state, &mut app, ticks[1][0].clone());
        assert!(state.screens.contains_key(&SessionKey::from_id(new)));
        assert!(!state.screens.contains_key(&SessionKey::from_id(old)));
        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::RekeyPty { from, to }
                if *from == SessionKey::from_id(old) && *to == SessionKey::from_id(new)
        )));
        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::Store(StoreIntent::SetMemberSession { token, session_id, .. })
                if token == "worker-1" && session_id == new
        )));
    }

    #[test]
    fn a_codex_directors_clear_is_followed_by_its_pane() {
        let (store, mut state, mut app, handles, (old, new)) =
            codex_member_that_ran_its_hook_under_another_thread(true, true, Some(5));

        let ticks = gather_move_ticks(&state, &store, &handles, &app, &the_log_ties(new), 2);

        let [
            Event::MemberSessionForked {
                token,
                old_id,
                new_id,
                ..
            },
        ] = ticks[1].as_slice()
        else {
            panic!("expected exactly one move: {:?}", ticks[1]);
        };
        assert_eq!(
            (token.as_str(), old_id.as_str(), new_id.as_str()),
            ("director", old, new)
        );
        apply(&mut state, &mut app, ticks[1][0].clone());
        let Stage::Brigade { director, .. } = &state.stage else {
            panic!("expected a staged brigade");
        };
        assert_eq!(director.as_ref(), Some(&SessionKey::from_id(new)));
    }

    #[test]
    fn a_hook_for_a_thread_the_log_does_not_tie_to_the_pane_moves_nothing_and_is_said_once() {
        let (store, state, app, handles, (_, new)) =
            codex_member_that_ran_its_hook_under_another_thread(false, true, Some(5));
        let n = UNCONFIRMED_NOTICE_TICKS as usize;

        // Another process wrote the thread last, or it has no rows at all.
        let ticks = gather_move_ticks(&state, &store, &handles, &app, &|_, _| false, n + 10);

        assert!(ticks.iter().all(|events| forked_events(events).is_empty()));
        let notices = notices_in(&ticks);
        assert_eq!(
            notices.len(),
            1,
            "said once, not once a second: {notices:?}"
        );
        assert!(
            notices[0].contains(new)
                && notices[0].contains("cannot yet tie")
                && notices[0].contains("does not show this pane's process writing it"),
            "{notices:?}"
        );
        // The boundary: the tick before the threshold says nothing, the threshold's
        // own tick says it. A log write that lags the hook is no cause for a line.
        assert!(
            ticks[..n - 1].iter().all(Vec::is_empty),
            "nothing before tick {n}"
        );
        assert_eq!(notices_in(&ticks[n - 1..n]).len(), 1, "said on tick {n}");
    }

    #[test]
    fn the_unconfirmed_notice_calls_a_brigade_by_its_name_or_else_its_number_never_by_empty_quotes()
    {
        let n = UNCONFIRMED_NOTICE_TICKS as usize;
        let said = |name: &str| {
            let (store, state, app, handles, _) =
                codex_member_in_brigade_named(name, false, true, Some(5));
            let ticks = gather_move_ticks(&state, &store, &handles, &app, &|_, _| false, n + 1);
            let brigade_id = store.borrow().list_brigades().unwrap()[0].id;
            (notices_in(&ticks), brigade_id)
        };

        let (notices, _) = said("cell");
        assert!(
            notices[0].starts_with("worker-1 of \"cell\": "),
            "{notices:?}"
        );
        let (notices, id) = said("");
        assert!(
            notices[0].starts_with(&format!("worker-1 of brigade {id}: ")),
            "{notices:?}"
        );
    }

    #[test]
    fn a_hook_for_a_thread_that_is_not_a_listed_session_moves_nothing_and_is_said_once() {
        // A sub-agent's thread may run the hook, with log rows naming the pane's
        // pid; only the session list tells it from the pane's own.
        let (store, state, app, handles, (_, new)) =
            codex_member_that_ran_its_hook_under_another_thread(false, false, Some(5));
        let n = UNCONFIRMED_NOTICE_TICKS as usize;

        let ticks = gather_move_ticks(&state, &store, &handles, &app, &the_log_ties(new), n + 10);

        assert!(ticks.iter().all(|events| forked_events(events).is_empty()));
        let notices = notices_in(&ticks);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].contains("not a listed Codex session"),
            "{notices:?}"
        );
        assert!(ticks[..n - 1].iter().all(Vec::is_empty));
        assert_eq!(notices_in(&ticks[n - 1..n]).len(), 1);
    }

    #[test]
    fn a_listed_session_that_is_not_a_codex_one_is_never_followed_onto() {
        // The id the hook reported is listed, but as a Claude session: a Codex pane
        // is not in it, whatever the log says.
        let (store, state, _, handles, (_, new)) =
            codex_member_that_ran_its_hook_under_another_thread(false, true, Some(5));
        let app = App::new(vec![
            test_row("dir", AgentKind::ClaudeCode),
            test_row("w1-old", AgentKind::Codex),
            test_row(new, AgentKind::ClaudeCode),
        ]);
        let n = UNCONFIRMED_NOTICE_TICKS as usize;

        let ticks = gather_move_ticks(&state, &store, &handles, &app, &the_log_ties(new), n);

        assert!(ticks.iter().all(|events| forked_events(events).is_empty()));
        assert_eq!(notices_in(&ticks).len(), 1);
    }

    #[test]
    fn an_unreadable_roster_does_not_reset_the_unconfirmed_count_or_say_the_notice_again() {
        let (store, state, app, handles, _) =
            codex_member_that_ran_its_hook_under_another_thread(false, true, Some(5));
        let dir = tempfile::tempdir().unwrap();
        let claude_home = ClaudeHome::new(dir.path().to_path_buf());
        let whole = TickRead::read(&store, &claude_home, None);
        let mut unread = TickRead::read(&store, &claude_home, None);
        unread.brigades.as_mut().unwrap()[0].1 = None;
        let mut watch = SessionMoveWatch::default();
        let view = ProcessView {
            probe: &SysinfoProbe,
            alive: &|_| true,
        };
        let sources = MoveSources {
            processes: &view,
            owns_thread: &|_, _| false,
        };
        let mut notices = |read: &TickRead, ticks: u32| -> usize {
            (0..ticks)
                .map(|_| {
                    let events = gather_session_moves(
                        read, &state, &handles, &app, &mut watch, &sources, None,
                    );
                    notices_in(&[events]).len()
                })
                .sum()
        };

        // One unreadable tick amid the count does not restart it...
        assert_eq!(notices(&whole, UNCONFIRMED_NOTICE_TICKS - 1), 0);
        assert_eq!(notices(&unread, 1), 0);
        assert_eq!(notices(&whole, 1), 1, "the count carried across the gap");
        // ...and one after the notice does not make it be said again.
        assert_eq!(notices(&unread, 1), 0);
        assert_eq!(notices(&whole, 5), 0, "the latch carried across the gap");
    }

    #[test]
    fn a_thread_the_log_ties_to_the_pane_late_is_followed_without_a_word() {
        let (store, state, app, handles, (_, new)) =
            codex_member_that_ran_its_hook_under_another_thread(false, true, Some(5));
        let asked = std::cell::Cell::new(0);

        let ticks = gather_move_ticks(
            &state,
            &store,
            &handles,
            &app,
            &|thread, pid| {
                asked.set(asked.get() + 1);
                asked.get() > 2 && the_log_ties(new)(thread, pid)
            },
            5,
        );

        assert_eq!(forked_events(&ticks[3]).len(), 1, "{ticks:?}");
        assert!(notices_in(&ticks).is_empty());
    }

    #[test]
    fn a_codex_move_onto_a_session_another_members_row_holds_is_marked_for_the_core_to_refuse() {
        let (store, mut state, mut app, handles, (old, new)) =
            codex_member_that_ran_its_hook_under_another_thread(false, true, Some(5));
        let brigade_id = store.borrow().list_brigades().unwrap()[0].id;
        store
            .borrow_mut()
            .add_brigade_member(
                brigade_id,
                "worker-2",
                BrigadeRole::Worker,
                Some(&SessionId(new.to_string())),
            )
            .unwrap();

        let ticks = gather_move_ticks(&state, &store, &handles, &app, &the_log_ties(new), 2);

        let [
            Event::MemberSessionForked {
                held_by_row: true, ..
            },
        ] = ticks[1].as_slice()
        else {
            panic!("expected the move marked as held by a row: {:?}", ticks[1]);
        };
        let cmds = apply(&mut state, &mut app, ticks[1][0].clone());
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::RekeyPty { .. } | Cmd::Store(_))),
            "nothing renamed, nothing written: {cmds:?}"
        );
        assert!(state.screens.contains_key(&SessionKey::from_id(old)));
        assert!(state.status.as_deref().unwrap().contains("already in use"));
    }

    #[test]
    fn a_codex_move_onto_a_session_a_pane_hosts_is_refused_by_the_core() {
        let (store, mut state, mut app, handles, (old, new)) =
            codex_member_that_ran_its_hook_under_another_thread(false, true, Some(5));
        // The operator opened the thread in a pane of its own before the hook ran.
        state
            .screens
            .insert(SessionKey::from_id(new), Screen::new(24, 80));

        let ticks = gather_move_ticks(&state, &store, &handles, &app, &the_log_ties(new), 2);

        let cmds = apply(&mut state, &mut app, ticks[1][0].clone());
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::RekeyPty { .. } | Cmd::Store(_))),
            "{cmds:?}"
        );
        assert!(state.screens.contains_key(&SessionKey::from_id(old)));
        assert!(state.status.as_deref().unwrap().contains("already in use"));
    }

    #[test]
    fn a_codex_move_the_core_refuses_does_not_ask_the_log_again_every_tick() {
        let (store, state, app, handles, (_, new)) =
            codex_member_that_ran_its_hook_under_another_thread(false, true, Some(5));
        let brigade_id = store.borrow().list_brigades().unwrap()[0].id;
        store
            .borrow_mut()
            .add_brigade_member(
                brigade_id,
                "worker-2",
                BrigadeRole::Worker,
                Some(&SessionId(new.to_string())),
            )
            .unwrap();
        let asked = std::cell::Cell::new(0);

        let ticks = gather_move_ticks(
            &state,
            &store,
            &handles,
            &app,
            &|thread, pid| {
                asked.set(asked.get() + 1);
                the_log_ties(new)(thread, pid)
            },
            8,
        );

        assert_eq!(asked.get(), 1);
        assert_eq!(
            ticks
                .iter()
                .map(|events| forked_events(events).len())
                .sum::<usize>(),
            2,
            "sent when believed and again after the resend interval, as for Claude"
        );
    }

    #[test]
    fn a_briefing_from_before_the_pane_was_spawned_is_not_a_move() {
        // The column is never cleared: an earlier process's value stays.
        let (store, state, app, handles, _) =
            codex_member_that_ran_its_hook_under_another_thread(false, true, Some(-60));

        let ticks = gather_move_ticks(
            &state,
            &store,
            &handles,
            &app,
            &|_, _| unreachable!("a stale briefing is never checked against the log"),
            15,
        );

        assert!(ticks.iter().all(Vec::is_empty), "{ticks:?}");
    }

    #[test]
    fn a_briefing_under_the_session_the_row_already_holds_is_not_a_move() {
        let (store, state, app, handles, (old, _)) =
            codex_member_that_ran_its_hook_under_another_thread(false, true, Some(5));
        let brigade_id = store.borrow().list_brigades().unwrap()[0].id;
        store
            .borrow_mut()
            .record_briefing(
                brigade_id,
                "worker-1",
                &SessionId(old.to_string()),
                SystemTime::now() + Duration::from_secs(5),
            )
            .unwrap();

        let ticks = gather_move_ticks(
            &state,
            &store,
            &handles,
            &app,
            &|_, _| unreachable!("nothing differs"),
            4,
        );

        assert!(ticks.iter().all(Vec::is_empty), "{ticks:?}");
    }

    #[test]
    fn a_claude_members_briefing_column_is_never_read_as_a_move() {
        let (store, state, _app) = staged_worker("w1-old", Some(AgentKind::ClaudeCode));
        let app = App::new(vec![
            test_row("dir", AgentKind::ClaudeCode),
            test_row("w1-old", AgentKind::ClaudeCode),
            test_row("w1-new", AgentKind::Codex),
        ]);
        let brigade_id = store.borrow().list_brigades().unwrap()[0].id;
        let host = MockPtyHost {
            pid: Some(CODEX_PID),
            ..Default::default()
        };
        let mut handles = HashMap::new();
        handles.insert(SessionKey::from_id("w1-old"), open(&host));
        store
            .borrow_mut()
            .record_briefing(
                brigade_id,
                "worker-1",
                &SessionId("w1-new".to_string()),
                SystemTime::now() + Duration::from_secs(5),
            )
            .unwrap();

        let ticks = gather_move_ticks(&state, &store, &handles, &app, &|_, _| true, 4);

        assert!(ticks.iter().all(Vec::is_empty), "{ticks:?}");
    }

    #[test]
    fn gather_session_moves_ignores_a_pane_with_no_known_child_pid() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1-old", Some(AgentKind::ClaudeCode));
        write_live_state(claude_home.path(), 4242, "w1-new", Path::new("/work/alpha"));

        // No handle registered at all for the pane's key, so its pid can
        // never be known — this must not panic, just find nothing.
        let handles = HashMap::new();

        let events = gather_session_moves_reading(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            &handles,
            &app,
            None,
        );

        assert!(events.is_empty());
    }

    /// A hosted Claude pane that is in no brigade, with the pane's own process
    /// (`pid`) and the row the list would have for it.
    fn solo_pane(
        id: &str,
        pid: u32,
    ) -> (
        RefCell<Store>,
        EmporiumState,
        App,
        HashMap<SessionKey, PtyHandle>,
        MockPtyHost,
    ) {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let mut state = EmporiumState::new(PrefixKey::default());
        state
            .screens
            .insert(SessionKey::from_id(id), Screen::new(24, 80));
        let app = App::new(vec![test_row(id, AgentKind::ClaudeCode)]);
        let host = MockPtyHost {
            pid: Some(pid),
            ..Default::default()
        };
        let mut handles = HashMap::new();
        handles.insert(SessionKey::from_id(id), open(&host));
        (store, state, app, handles, host)
    }

    #[test]
    fn a_solo_pane_whose_process_reports_another_session_is_named_as_moved() {
        // `/clear` and `/branch` both read like this: the pane's own process
        // publishes a different `sessionId`, still under its pid.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app, handles, _host) = solo_pane("solo-old", 4242);
        write_live_state(
            claude_home.path(),
            4242,
            "solo-new",
            Path::new("/work/alpha"),
        );

        let events = gather_session_moves_reading(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            &handles,
            &app,
            None,
        );

        assert!(
            matches!(
                events.as_slice(),
                [Event::PaneSessionMoved { old_id, new_id, held_by_row: false }]
                    if old_id == "solo-old" && new_id == "solo-new"
            ),
            "expected one move: {events:?}"
        );
    }

    #[test]
    fn a_solo_pane_whose_process_still_reports_its_key_is_silent() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app, handles, _host) = solo_pane("solo", 4242);
        write_live_state(claude_home.path(), 4242, "solo", Path::new("/work/alpha"));

        let events = gather_session_moves_reading(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            &handles,
            &app,
            None,
        );

        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn a_solo_codex_pane_and_a_pane_with_no_row_are_never_named_as_moved() {
        let claude_home = tempfile::tempdir().unwrap();
        write_live_state(claude_home.path(), 4242, "other", Path::new("/work/alpha"));
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let (store, state, _app, handles, _host) = solo_pane("solo", 4242);

        let codex = App::new(vec![test_row("solo", AgentKind::Codex)]);
        assert!(
            gather_session_moves_reading(&state, &store, &home, &handles, &codex, None).is_empty()
        );
        let no_row = App::new(Vec::new());
        assert!(
            gather_session_moves_reading(&state, &store, &home, &handles, &no_row, None).is_empty()
        );
    }

    #[test]
    fn a_brigade_member_is_named_by_the_member_event_and_never_as_a_solo_pane() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1-old", Some(AgentKind::ClaudeCode));
        write_live_state(claude_home.path(), 4242, "w1-new", Path::new("/work/alpha"));
        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        let mut handles = HashMap::new();
        handles.insert(SessionKey::from_id("w1-old"), open(&host));

        let events = gather_session_moves_reading(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            &handles,
            &app,
            None,
        );

        assert!(
            matches!(events.as_slice(), [Event::MemberSessionForked { .. }]),
            "{events:?}"
        );
    }

    /// The staged worker `w1-old` with its process (pid 4242) and a live file
    /// naming `w1-new` — a member that `/clear`ed.
    fn moved_member(
        claude_home: &Path,
    ) -> (
        RefCell<Store>,
        EmporiumState,
        App,
        HashMap<SessionKey, PtyHandle>,
        MockPtyHost,
    ) {
        let (store, state, app) = staged_worker("w1-old", Some(AgentKind::ClaudeCode));
        write_live_state(claude_home, 4242, "w1-new", Path::new("/work/alpha"));
        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        let mut handles = HashMap::new();
        handles.insert(SessionKey::from_id("w1-old"), open(&host));
        (store, state, app, handles, host)
    }

    #[test]
    fn a_move_is_believed_only_when_it_holds_for_two_ticks() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app, handles, _host) = moved_member(claude_home.path());
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut watch = SessionMoveWatch::default();

        let first =
            gather_session_moves_tick(&mut watch, &[], &state, &store, &home, &handles, &app);
        let second =
            gather_session_moves_tick(&mut watch, &[], &state, &store, &home, &handles, &app);

        assert!(first.is_empty(), "one tick is not enough: {first:?}");
        assert!(
            matches!(second.as_slice(), [Event::MemberSessionForked { new_id, .. }] if new_id == "w1-new"),
            "{second:?}"
        );
    }

    #[test]
    fn a_live_file_missing_for_one_tick_cannot_rename_the_pane_onto_a_nested_session() {
        // The pane's claude (pid 20) runs under the pane's process (pid 10) and
        // has started a `claude -p` (pid 30) of its own. Pid 20's file is
        // unreadable for a tick, so the nested session is the only entry under
        // the pane: that one tick alone looks like a move onto it.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1-old", Some(AgentKind::ClaudeCode));
        let host = MockPtyHost {
            pid: Some(10),
            ..Default::default()
        };
        let mut handles = HashMap::new();
        handles.insert(SessionKey::from_id("w1-old"), open(&host));
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let tree = Tree(vec![(20, 10), (30, 20)]);
        let view = ProcessView {
            probe: &tree,
            alive: &|_| true,
        };
        let mut watch = SessionMoveWatch::default();
        let cwd = Path::new("/work/alpha");
        let tick = |watch: &mut SessionMoveWatch| {
            gather_session_moves_viewing(watch, &view, &state, &store, &home, &handles, &app)
        };

        // Tick 1: only the nested session's file is there.
        write_live_state(claude_home.path(), 30, "nested", cwd);
        let first = tick(&mut watch);
        assert_eq!(
            watch.seen.values().map(|w| w.ticks).collect::<Vec<_>>(),
            [1]
        );
        // Tick 2: the pane's own file is back and says what it always said.
        write_live_state(claude_home.path(), 20, "w1-old", cwd);
        let second = tick(&mut watch);
        assert!(
            watch.seen.is_empty(),
            "nothing moved, so nothing is watched"
        );
        // Tick 3: the nested session shows up alone again.
        std::fs::remove_file(claude_home.path().join("sessions").join("20.json")).unwrap();
        let third = tick(&mut watch);

        assert!(first.is_empty() && second.is_empty() && third.is_empty());
        assert_eq!(
            watch.seen.values().map(|w| w.ticks).collect::<Vec<_>>(),
            [1],
            "the streak started over; carried across tick 2 it would have been sent here"
        );
    }

    #[test]
    fn a_live_file_whose_process_is_gone_names_no_move() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app, handles, _host) = moved_member(claude_home.path());
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut watch = SessionMoveWatch::default();

        for _ in 0..3 {
            let events = gather_session_moves_tick(
                &mut watch,
                &[4242],
                &state,
                &store,
                &home,
                &handles,
                &app,
            );
            assert!(events.is_empty(), "{events:?}");
        }
    }

    #[test]
    fn a_move_that_stays_observed_is_sent_again_every_few_ticks() {
        // What a refused move looks like from here: the pane stays under its old
        // key and the process keeps reporting the new id.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app, handles, _host) = moved_member(claude_home.path());
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut watch = SessionMoveWatch::default();

        let sent_on: Vec<usize> = (1..=13)
            .filter(|_| {
                let events = gather_session_moves_tick(
                    &mut watch,
                    &[],
                    &state,
                    &store,
                    &home,
                    &handles,
                    &app,
                );
                !events.is_empty()
            })
            .collect();

        let first = MOVE_CONFIRM_TICKS as usize;
        let every = MOVE_RESEND_TICKS as usize;
        assert_eq!(sent_on, [first, first + every, first + 2 * every]);
        // The process moves on: that is a new fact, believed after two ticks.
        write_live_state(
            claude_home.path(),
            4242,
            "w1-third",
            Path::new("/work/alpha"),
        );
        let next =
            gather_session_moves_tick(&mut watch, &[], &state, &store, &home, &handles, &app);
        assert!(next.is_empty());
        let next =
            gather_session_moves_tick(&mut watch, &[], &state, &store, &home, &handles, &app);
        assert!(
            matches!(next.as_slice(), [Event::MemberSessionForked { new_id, .. }] if new_id == "w1-third"),
            "{next:?}"
        );
    }

    #[test]
    fn a_refused_move_is_renamed_within_a_few_ticks_of_the_hold_letting_go() {
        // The regression this guards: a conversation another pane holds is
        // resumed from inside this pane. The core refuses, the other pane closes,
        // and with the move sent only once this pane would stay on its old key
        // for good (the same goes for an open that fails, which the core tests
        // cover: `pending_opens` is not reachable from here).
        let claude_home = tempfile::tempdir().unwrap();
        let (store, mut state, app) = staged_worker("w1-old", Some(AgentKind::ClaudeCode));
        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        let mut handles = HashMap::new();
        handles.insert(SessionKey::from_id("w1-old"), open(&host));
        write_live_state(claude_home.path(), 4242, "p", Path::new("/work/alpha"));
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut watch = SessionMoveWatch::default();
        let mut core_app = App::new(Vec::new());
        state
            .screens
            .insert(SessionKey::from_id("p"), Screen::new(24, 80));
        let mut now = Instant::now();
        let mut renamed_on = None;

        for tick in 1..=20 {
            if tick == 8 {
                // The other pane closed: nothing holds `p` any more.
                state.screens.remove(&SessionKey::from_id("p"));
            }
            for event in
                gather_session_moves_tick(&mut watch, &[], &state, &store, &home, &handles, &app)
            {
                now += Duration::from_secs(1);
                let cmds = engine::update(
                    &mut state,
                    &mut core_app,
                    &BrigadeConfig::default(),
                    event,
                    now,
                );
                if cmds.iter().any(|cmd| matches!(cmd, Cmd::RekeyPty { .. })) {
                    renamed_on = Some(tick);
                }
            }
            if renamed_on.is_some() {
                break;
            }
        }

        let renamed_on = renamed_on.expect("the pane is renamed once the hold is gone");
        assert!(
            renamed_on > 8 && renamed_on <= 8 + MOVE_RESEND_TICKS,
            "renamed on tick {renamed_on}"
        );
        assert!(state.screens.contains_key(&SessionKey::from_id("p")));
    }

    #[test]
    fn the_watch_forgets_a_pane_that_is_gone() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, mut state, app, handles, _host) = moved_member(claude_home.path());
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut watch = SessionMoveWatch::default();
        gather_session_moves_tick(&mut watch, &[], &state, &store, &home, &handles, &app);
        gather_session_moves_tick(&mut watch, &[], &state, &store, &home, &handles, &app);
        assert_eq!(watch.seen.len(), 1);

        state.screens.remove(&SessionKey::from_id("w1-old"));
        gather_session_moves_tick(&mut watch, &[], &state, &store, &home, &handles, &app);

        assert!(watch.seen.is_empty());
    }

    #[test]
    fn a_move_onto_an_id_another_members_row_holds_is_marked_held() {
        // The other member's pane is closed (no pane holds `held`), but its row
        // records it, so the member and the solo event both say so.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app, handles, _host) = moved_member(claude_home.path());
        write_live_state(claude_home.path(), 4242, "held", Path::new("/work/alpha"));
        let other = store.borrow_mut().create_brigade("other").unwrap();
        store
            .borrow_mut()
            .add_brigade_member(
                other,
                "worker-1",
                BrigadeRole::Worker,
                Some(&SessionId("held".to_string())),
            )
            .unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut watch = SessionMoveWatch::default();
        gather_session_moves_tick(&mut watch, &[], &state, &store, &home, &handles, &app);

        let events =
            gather_session_moves_tick(&mut watch, &[], &state, &store, &home, &handles, &app);

        assert!(
            matches!(
                events.as_slice(),
                [Event::MemberSessionForked { new_id, held_by_row: true, .. }] if new_id == "held"
            ),
            "{events:?}"
        );

        // A pane that is in no brigade, moving onto the same id.
        let (solo_store, solo_state, solo_app, solo_handles, _solo_host) = solo_pane("solo", 4242);
        let brigade = solo_store.borrow_mut().create_brigade("holder").unwrap();
        solo_store
            .borrow_mut()
            .add_brigade_member(
                brigade,
                "worker-1",
                BrigadeRole::Worker,
                Some(&SessionId("held".to_string())),
            )
            .unwrap();
        let mut solo_watch = SessionMoveWatch::default();
        gather_session_moves_tick(
            &mut solo_watch,
            &[],
            &solo_state,
            &solo_store,
            &home,
            &solo_handles,
            &solo_app,
        );
        let solo = gather_session_moves_tick(
            &mut solo_watch,
            &[],
            &solo_state,
            &solo_store,
            &home,
            &solo_handles,
            &solo_app,
        );
        assert!(
            matches!(
                solo.as_slice(),
                [Event::PaneSessionMoved { new_id, held_by_row: true, .. }] if new_id == "held"
            ),
            "{solo:?}"
        );
    }

    #[test]
    fn a_members_own_row_is_not_another_rows_hold() {
        // The row that holds `w1-new` is the member's own (the store already
        // follows the move): not a reason to refuse.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, _state, _app, _handles, _host) = moved_member(claude_home.path());
        let brigade_id = store.borrow().list_brigades().unwrap()[0].id;
        store
            .borrow_mut()
            .set_member_session(brigade_id, "worker-1", &SessionId("w1-new".to_string()))
            .unwrap();
        let read = TickRead::read(
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            None,
        );
        let roster = read.brigades.as_deref().unwrap();

        assert!(!row_holds(roster, "w1-new", Some((brigade_id, "worker-1"))));
        assert!(row_holds(roster, "w1-new", None));
    }

    /// Three ticks of the gather, enough for any move to have held, against a
    /// roster read once.
    fn moves_over_three_ticks(
        read: &TickRead,
        state: &EmporiumState,
        handles: &HashMap<SessionKey, PtyHandle>,
        app: &App,
    ) -> Vec<Event> {
        let mut watch = SessionMoveWatch::default();
        let view = ProcessView {
            probe: &SysinfoProbe,
            alive: &|_| true,
        };
        (0..3)
            .flat_map(|_| {
                gather_session_moves(
                    read,
                    state,
                    handles,
                    app,
                    &mut watch,
                    &MoveSources {
                        processes: &view,
                        owns_thread: &|_, _| false,
                    },
                    None,
                )
            })
            .collect()
    }

    #[test]
    fn a_tick_that_could_not_read_one_brigades_roster_names_no_solo_move() {
        // One of two brigades' members failed to read. A pane in none of the
        // rosters that were read might be one of that brigade's, whose row would
        // then name the id it left.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app, handles, _host) = solo_pane("solo-old", 4242);
        write_live_state(
            claude_home.path(),
            4242,
            "solo-new",
            Path::new("/work/alpha"),
        );
        store.borrow_mut().create_brigade("readable").unwrap();
        store.borrow_mut().create_brigade("unreadable").unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut read = TickRead::read(&store, &home, None);

        let complete = moves_over_three_ticks(&read, &state, &handles, &app);
        assert_eq!(
            complete.len(),
            1,
            "the roster as read is complete: {complete:?}"
        );

        read.brigades.as_mut().unwrap()[1].1 = None;
        let events = moves_over_three_ticks(&read, &state, &handles, &app);
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn a_roster_that_stays_unread_is_told_once_per_stretch() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app, handles, _host) = solo_pane("solo", 4242);
        store.borrow_mut().create_brigade("unreadable").unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let complete = TickRead::read(&store, &home, None);
        let mut unread = TickRead::read(&store, &home, None);
        unread.brigades.as_mut().unwrap()[0].1 = None;
        let mut watch = SessionMoveWatch::default();
        let view = ProcessView {
            probe: &SysinfoProbe,
            alive: &|_| true,
        };
        let mut notices = |read: &TickRead, ticks: u32| -> usize {
            (0..ticks)
                .flat_map(|_| {
                    gather_session_moves(
                        read,
                        &state,
                        &handles,
                        &app,
                        &mut watch,
                        &MoveSources {
                            processes: &view,
                            owns_thread: &|_, _| false,
                        },
                        None,
                    )
                })
                .filter(|event| matches!(event, Event::StartupNotices { .. }))
                .count()
        };

        assert_eq!(
            notices(&unread, MOVE_PAUSED_NOTICE_TICKS - 1),
            0,
            "a short gap is routine"
        );
        assert_eq!(notices(&unread, 1), 1, "said when it has lasted");
        assert_eq!(notices(&unread, 30), 0, "once per stretch");
        assert_eq!(notices(&complete, 3), 0);
        assert_eq!(
            notices(&unread, MOVE_PAUSED_NOTICE_TICKS),
            1,
            "a complete tick ended the stretch, so the next one is told again"
        );
    }

    #[test]
    fn a_tick_that_could_not_read_one_brigades_roster_names_no_member_move() {
        // The member's move is onto `held`, which the unreadable brigade's member
        // may hold: with that row unread, `held_by_row` would read false and the
        // core would rename the pane and write the row, taking the id from it.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app, handles, _host) = moved_member(claude_home.path());
        write_live_state(claude_home.path(), 4242, "held", Path::new("/work/alpha"));
        let other = store.borrow_mut().create_brigade("unreadable").unwrap();
        store
            .borrow_mut()
            .add_brigade_member(
                other,
                "worker-1",
                BrigadeRole::Worker,
                Some(&SessionId("held".to_string())),
            )
            .unwrap();
        let home = ClaudeHome::new(claude_home.path().to_path_buf());
        let mut read = TickRead::read(&store, &home, None);

        let complete = moves_over_three_ticks(&read, &state, &handles, &app);
        assert!(
            matches!(
                complete.as_slice(),
                [Event::MemberSessionForked {
                    held_by_row: true,
                    ..
                }]
            ),
            "{complete:?}"
        );

        let unreadable = read
            .brigades
            .as_mut()
            .unwrap()
            .iter_mut()
            .find(|(brigade, _)| brigade.id == other)
            .unwrap();
        unreadable.1 = None;
        let events = moves_over_three_ticks(&read, &state, &handles, &app);
        assert!(events.is_empty(), "{events:?}");
    }

    /// A process table of `(pid, parent)` pairs.
    struct Tree(Vec<(u32, u32)>);

    impl ProcessProbe for Tree {
        fn is_alive(&self, _pid: u32) -> bool {
            true
        }
        fn parent_pid(&self, pid: u32) -> Option<u32> {
            self.0
                .iter()
                .find(|(child, _)| *child == pid)
                .map(|(_, parent)| *parent)
        }
        fn is_alive_matching(&self, _pid: u32, _proc_start: &str) -> bool {
            true
        }
    }

    fn reporting(pid: u32, session_id: &str) -> LiveSession {
        LiveSession {
            pid,
            session_id: Some(session_id.to_string()),
            cwd: None,
            status: None,
            waiting_for: None,
            kind: None,
            name: None,
            proc_start: None,
            version: None,
        }
    }

    #[test]
    fn the_nearest_process_under_the_pane_names_the_new_session() {
        // The pane's claude (pid 20, behind a shim, pid 10 is the pane's child)
        // started a `claude -p` (pid 30) of its own; both are below the pane,
        // and the nearer one is the pane's.
        let tree = Tree(vec![(20, 10), (30, 20)]);
        let live = vec![reporting(30, "nested"), reporting(20, "cleared")];
        let view = ProcessView {
            probe: &tree,
            alive: &|_| true,
        };

        assert_eq!(
            moved_session_id(&live, "old", 10, &view, None),
            Some((20, "cleared".to_string()))
        );
        // Still reporting its key: nothing moved, whatever else is below it.
        let live = vec![reporting(30, "nested"), reporting(20, "old")];
        assert_eq!(moved_session_id(&live, "old", 10, &view, None), None);
        // An unrelated claude is not under the pane at all.
        let live = vec![reporting(99, "elsewhere")];
        assert_eq!(moved_session_id(&live, "old", 10, &view, None), None);
    }

    #[test]
    fn a_live_file_left_by_a_dead_process_is_not_a_report() {
        // The pane's claude (pid 20) reports nothing readable this tick; the file a
        // finished `claude -p` (pid 30, below it) left behind is all there is.
        let tree = Tree(vec![(20, 10), (30, 20)]);
        let live = vec![reporting(30, "nested")];
        let pid30_gone = ProcessView {
            probe: &tree,
            alive: &|pid| pid != 30,
        };
        let all_alive = ProcessView {
            probe: &tree,
            alive: &|_| true,
        };

        assert_eq!(moved_session_id(&live, "old", 10, &pid30_gone, None), None);
        assert_eq!(
            moved_session_id(&live, "old", 10, &all_alive, None),
            Some((30, "nested".to_string())),
            "the same file from a live process is a report"
        );
    }

    #[test]
    fn a_dead_file_still_naming_the_panes_own_id_does_not_hide_a_move() {
        // The pane's own file (pid 15, gone) still says `old`; the live process
        // under the pane (pid 20) has cleared to `new`. Counting the dead file as
        // the pane still reporting its key would hold the move back for as long as
        // the leftover lay there.
        let tree = Tree(vec![(15, 10), (20, 10)]);
        let live = vec![reporting(15, "old"), reporting(20, "new")];
        let view = ProcessView {
            probe: &tree,
            alive: &|pid| pid != 15,
        };

        assert_eq!(
            moved_session_id(&live, "old", 10, &view, None),
            Some((20, "new".to_string()))
        );
        let all_alive = ProcessView {
            probe: &tree,
            alive: &|_| true,
        };
        assert_eq!(
            moved_session_id(&live, "old", 10, &all_alive, None),
            None,
            "a live process still on its key means nothing moved"
        );
    }

    #[test]
    fn a_named_entry_is_trusted_without_walking_while_it_is_alive_and_says_the_same() {
        // `Tree` knows no parents, so a walk finds nothing: only the carried entry
        // can answer, and it must not once the process is gone or has moved again.
        let tree = Tree(Vec::new());
        let live = vec![reporting(20, "cleared")];
        let held = Some((20, "cleared"));
        let alive = ProcessView {
            probe: &tree,
            alive: &|_| true,
        };
        let gone = ProcessView {
            probe: &tree,
            alive: &|_| false,
        };

        assert_eq!(
            moved_session_id(&live, "old", 10, &alive, held),
            Some((20, "cleared".to_string()))
        );
        assert_eq!(
            moved_session_id(&live, "old", 10, &gone, held),
            None,
            "the process is gone"
        );
        let moved_again = vec![reporting(20, "third")];
        assert_eq!(
            moved_session_id(&moved_again, "old", 10, &alive, held),
            None,
            "it reports another id now, which the walk (finding no ancestry here) cannot place"
        );
    }

    #[test]
    fn a_pane_with_no_live_file_walks_each_pid_once_however_many_entries_ask() {
        // Panes without a file of their own never stop at the steady-state check:
        // every entry's chain is walked for each of them.
        struct Counting(RefCell<HashMap<u32, u32>>);
        impl ProcessProbe for Counting {
            fn is_alive(&self, _pid: u32) -> bool {
                true
            }
            fn parent_pid(&self, pid: u32) -> Option<u32> {
                *self.0.borrow_mut().entry(pid).or_insert(0) += 1;
                Some(pid + 1)
            }
            fn is_alive_matching(&self, _pid: u32, _proc_start: &str) -> bool {
                true
            }
        }
        let counting = Counting(RefCell::new(HashMap::new()));
        let cached = CachedParents {
            inner: &counting,
            parents: RefCell::new(HashMap::new()),
        };
        let view = ProcessView {
            probe: &cached,
            alive: &|_| true,
        };
        let live = vec![reporting(100, "a"), reporting(101, "b")];

        for pane in [1, 2, 3] {
            moved_session_id(&live, "none", pane, &view, None);
        }

        assert!(
            counting.0.borrow().values().all(|asked| *asked == 1),
            "{:?}",
            counting.0.borrow()
        );
    }

    // --- relay observations: gather_relay_observations ---------------------

    /// A minimal, correctly-attributed [`SessionRow`] for `id` — the "already
    /// discovered" fact `gather_relay_observations` resolves a member's
    /// product from, mirroring how `session::rows_from_metas` would have
    /// built it.
    fn test_row(id: &str, agent: AgentKind) -> SessionRow {
        SessionRow {
            id: id.to_string(),
            agent,
            title: None,
            cwd: None,
            dialog_open: false,
            activity: Activity::Alive,
            is_agent: false,
            preview: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
            archived: false,
        }
    }

    #[test]
    fn waiting_count_unions_list_rows_and_staged_tiles() {
        let mut listed = test_row("listed", AgentKind::ClaudeCode);
        listed.activity = Activity::Waiting;
        let mut hidden_staged = test_row("hidden-staged", AgentKind::ClaudeCode);
        hidden_staged.activity = Activity::Waiting;
        let mut app = App::new(vec![listed, hidden_staged])
            .with_hidden_member_ids(["hidden-staged".to_string()].into_iter().collect());
        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Brigade {
            id: 1,
            director: None,
            // `listed` occurs in both surfaces; the hidden member is tile-only
            // in the list. The unloaded pane deliberately has no row to count.
            panes: vec![
                SessionKey::from_id("listed"),
                SessionKey::from_id("hidden-staged"),
                SessionKey::from_id("not-loaded"),
            ],
            focused: 0,
        };

        engine::update(
            &mut state,
            &mut app,
            &BrigadeConfig::default(),
            Event::Resized {
                width: 80,
                height: 24,
            },
            Instant::now(),
        );
        assert_eq!(state.visible_waiting_count(), 2);
        let expected_attention_panes: HashSet<_> = [
            SessionKey::from_id("listed"),
            SessionKey::from_id("hidden-staged"),
        ]
        .into_iter()
        .collect();
        assert_eq!(state.attention_panes(), &expected_attention_panes);
        app.set_hidden_member_ids(["listed".to_string()].into_iter().collect());
        engine::update(
            &mut state,
            &mut app,
            &BrigadeConfig::default(),
            Event::Resized {
                width: 80,
                height: 24,
            },
            Instant::now(),
        );
        assert_eq!(state.visible_waiting_count(), 2);
    }

    #[test]
    fn focused_border_wins_over_waiting() {
        assert_eq!(border_style(true, true), border_style(true, false));
        assert_eq!(
            border_style(false, true),
            Style::default().fg(WAITING_ACTIVITY_COLOR)
        );
    }

    #[test]
    fn pane_title_pads_the_text_on_both_sides_whether_focused_or_not() {
        assert_eq!(pane_title("director", false).content, " director ");
        assert_eq!(pane_title("director", true).content, " director ");
    }

    #[test]
    fn focused_pane_title_is_reversed_video_in_the_focus_colour() {
        assert_eq!(
            pane_title("director", true).style,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::REVERSED)
        );
    }

    #[test]
    fn unfocused_pane_title_is_a_slate_tab_with_no_modifier() {
        // Black on grey, so it is not dimmed and carries no REVERSED that would
        // let it compete with the focused tab.
        assert_eq!(pane_title("director", false).style, slate());
    }

    /// An unfocused tile's label: black on grey, no modifier.
    fn slate() -> Style {
        Style::default().fg(Color::Black).bg(Color::Gray)
    }

    /// Draw a bordered block carrying `title` the way the pane frame does and
    /// return the cell holding the title's first letter. The title is written
    /// over the border row, so whatever its own style leaves unset is
    /// inherited from the border's.
    fn drawn_title_cell(focused: bool) -> ratatui::buffer::Cell {
        use ratatui::widgets::Widget as _;
        let area = Rect::new(0, 0, 30, 5);
        let mut buf = Buffer::empty(area);
        Block::bordered()
            .title(pane_title("director", focused))
            .border_style(border_style(focused, false))
            .render(area, &mut buf);
        // Row 0: the corner, then ` director ` — the `d` is at column 2.
        let cell = buf[(2, 0)].clone();
        assert_eq!(cell.symbol(), "d", "fixture check: found the title text");
        cell
    }

    #[test]
    fn an_unfocused_title_drawn_on_the_dark_gray_border_is_not_dark_gray_or_dim() {
        // The bug: a title with no foreground of its own kept the border's
        // `DarkGray` and was then dimmed, which is near black on a dark
        // terminal theme (operator report, 2026-11-10).
        assert_eq!(
            border_style(false, false),
            Style::default().fg(Color::DarkGray),
            "fixture check: the unfocused border really is DarkGray"
        );
        let cell = drawn_title_cell(false);
        assert_ne!(cell.fg, Color::DarkGray);
        assert_eq!((cell.fg, cell.bg), (Color::Black, Color::Gray));
        assert!(!cell.modifier.contains(Modifier::DIM));
    }

    #[test]
    fn a_focused_title_drawn_on_the_border_is_still_reversed_cyan() {
        let cell = drawn_title_cell(true);
        assert_eq!(cell.fg, Color::Cyan);
        assert!(cell.modifier.contains(Modifier::REVERSED));
    }

    /// A staged Director ("dir", Claude) plus one Worker at
    /// `worker_session_id`, whose product is `worker_agent` in both the
    /// store's membership and the `App`'s row list — the shape
    /// `gather_relay_observations` reads. `rows` are appended beyond the two
    /// default ones for the "unresolved product" case, where the Worker's id
    /// deliberately has no row at all.
    fn staged_worker(
        worker_session_id: &str,
        worker_agent: Option<AgentKind>,
    ) -> (RefCell<Store>, EmporiumState, App) {
        staged_worker_in(worker_session_id, worker_agent, "cell")
    }

    fn staged_worker_in(
        worker_session_id: &str,
        worker_agent: Option<AgentKind>,
        brigade_name: &str,
    ) -> (RefCell<Store>, EmporiumState, App) {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade(brigade_name).unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "director",
                BrigadeRole::Director,
                Some(&SessionId("dir".to_string())),
            )
            .unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "worker-1",
                BrigadeRole::Worker,
                Some(&SessionId(worker_session_id.to_string())),
            )
            .unwrap();
        store
            .enqueue_brigade_message(brigade_id, "worker-1", BrigadeRole::Director, None, "hi")
            .unwrap();

        let mut state = EmporiumState::new(PrefixKey::default());
        state
            .screens
            .insert(SessionKey::from_id("dir"), Screen::new(24, 80));
        state
            .screens
            .insert(SessionKey::from_id(worker_session_id), Screen::new(24, 80));
        state.stage = Stage::Brigade {
            id: brigade_id,
            director: Some(SessionKey::from_id("dir")),
            panes: vec![
                SessionKey::from_id("dir"),
                SessionKey::from_id(worker_session_id),
            ],
            focused: 0,
        };
        let mut rows = vec![test_row("dir", AgentKind::ClaudeCode)];
        if let Some(agent) = worker_agent {
            rows.push(test_row(worker_session_id, agent));
        }
        let app = App::new(rows);
        (RefCell::new(store), state, app)
    }

    /// Writes a synthetic `threads` row (`state_5.sqlite`) pointing
    /// `thread_id` at a rollout file under `codex_home`, and writes that
    /// rollout file with `event_msg` lines built from `markers` (each
    /// `"task_started"` or `"task_complete"`) — the shape
    /// `codex_activity::is_thread_idle` reads.
    fn write_codex_rollout(codex_home: &Path, thread_id: &str, markers: &[&str]) {
        std::fs::create_dir_all(codex_home).unwrap();
        let rollout_path = codex_home.join(format!("{thread_id}.jsonl"));
        let body: String = markers
            .iter()
            .map(|marker| {
                format!(r#"{{"type":"event_msg","payload":{{"type":"{marker}"}}}}"#) + "\n"
            })
            .collect();
        std::fs::write(&rollout_path, body).unwrap();

        let conn = rusqlite::Connection::open(codex_home.join("state_5.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL)",
        )
        .ok(); // no-op once the table exists
        conn.execute(
            "INSERT INTO threads (id, rollout_path) VALUES (?1, ?2)",
            rusqlite::params![thread_id, rollout_path.to_string_lossy().to_string()],
        )
        .unwrap();
    }

    #[test]
    fn a_codex_member_mid_turn_is_not_idle() {
        let codex_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", Some(AgentKind::Codex));
        write_codex_rollout(codex_home.path(), "w1", &["task_started"]);

        let claude_home = tempfile::tempdir().unwrap();
        let observations = gather_relay_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            Some(&CodexHome::new(codex_home.path().to_path_buf())),
            &app,
            None,
            &mut GlanceGate::default(),
        );

        let worker = observations
            .iter()
            .find(|o| o.token == "worker-1")
            .expect("worker-1 has an open pane and a known session id");
        assert_eq!(worker.is_idle_this_tick, Some(false));
    }

    #[test]
    fn a_codex_member_past_task_complete_is_idle() {
        let codex_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", Some(AgentKind::Codex));
        write_codex_rollout(codex_home.path(), "w1", &["task_started", "task_complete"]);

        let claude_home = tempfile::tempdir().unwrap();
        let observations = gather_relay_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            Some(&CodexHome::new(codex_home.path().to_path_buf())),
            &app,
            None,
            &mut GlanceGate::default(),
        );

        let worker = observations.iter().find(|o| o.token == "worker-1").unwrap();
        assert_eq!(worker.is_idle_this_tick, Some(true));
    }

    #[test]
    fn a_claude_members_idle_detection_is_unchanged_by_the_codex_split() {
        // The live-file-based check, reached via the AgentKind::ClaudeCode arm
        // beside Codex's own — the alive-and-busy and alive-and-waiting cases
        // are their own tests below, not this one.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        write_live_state(
            claude_home.path(),
            std::process::id(),
            "w1",
            Path::new("/work"),
        );

        let observations = gather_relay_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            None,
            &app,
            None,
            &mut GlanceGate::default(),
        );
        let worker = observations.iter().find(|o| o.token == "worker-1").unwrap();
        assert_eq!(
            worker.is_idle_this_tick,
            Some(true),
            "this process's own pid is alive and the live file reports no busy status"
        );
    }

    #[test]
    fn relay_nudges_an_idle_member_of_an_offstage_brigade() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, mut state, _) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        let offstage_brigade = store.borrow_mut().create_brigade("offstage").unwrap();
        store
            .borrow_mut()
            .add_brigade_member(
                offstage_brigade,
                "director",
                BrigadeRole::Director,
                Some(&SessionId("offstage-director".to_string())),
            )
            .unwrap();
        store
            .borrow()
            .enqueue_brigade_message(
                offstage_brigade,
                "director",
                BrigadeRole::Director,
                None,
                "mail",
            )
            .unwrap();
        let offstage_key = SessionKey::from_id("offstage-director");
        state
            .screens
            .insert(offstage_key.clone(), Screen::new(24, 80));
        let mut app = App::new(vec![
            test_row("dir", AgentKind::ClaudeCode),
            test_row("w1", AgentKind::ClaudeCode),
            test_row("offstage-director", AgentKind::ClaudeCode),
        ]);
        write_live_state(
            claude_home.path(),
            std::process::id(),
            "offstage-director",
            Path::new("/work"),
        );

        let relay = gather_relay_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            None,
            &app,
            None,
            &mut GlanceGate::default(),
        );
        assert!(relay.iter().any(|observation| {
            observation.brigade_id == offstage_brigade
                && observation.key == offstage_key
                && observation.has_unseen
                && observation.is_idle_this_tick == Some(true)
        }));

        let now = Instant::now();
        engine::update(
            &mut state,
            &mut app,
            &BrigadeConfig::default(),
            Event::Tick {
                relay: relay.clone(),
                brigades: Vec::new(),
            },
            now,
        );
        let cmds = engine::update(
            &mut state,
            &mut app,
            &BrigadeConfig::default(),
            Event::Tick {
                relay,
                brigades: Vec::new(),
            },
            now + Duration::from_secs(1),
        );
        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::WritePty { key, bytes }
                if key == &offstage_key
                    && bytes
                        == b"[banto relay] Your brigade peer sent you a message. Call the check_messages tool now."
        )));
    }

    #[test]
    fn stored_claude_agent_keeps_an_unlisted_worker_relay_eligible() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", None);
        store
            .borrow_mut()
            .set_member_agent(1, "worker-1", AgentKind::ClaudeCode)
            .unwrap();
        write_live_state(
            claude_home.path(),
            std::process::id(),
            "w1",
            Path::new("/work"),
        );

        let observations = gather_relay_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            None,
            &app,
            None,
            &mut GlanceGate::default(),
        );
        let worker = observations.iter().find(|o| o.token == "worker-1").unwrap();
        assert_eq!(worker.is_idle_this_tick, Some(true));
    }

    #[test]
    fn a_claude_member_reported_busy_is_not_idle() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        write_live_state_with_status(
            claude_home.path(),
            std::process::id(),
            "w1",
            Path::new("/work"),
            "busy",
        );

        let observations = gather_relay_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            None,
            &app,
            None,
            &mut GlanceGate::default(),
        );
        let worker = observations.iter().find(|o| o.token == "worker-1").unwrap();
        assert_eq!(worker.is_idle_this_tick, Some(false));
    }

    #[test]
    fn a_claude_member_reported_waiting_is_not_idle() {
        // The regression this pins: a member sitting at a permission or
        // plan-mode prompt reports `status: "waiting"` (see
        // `LIVE_STATUS_WAITING`'s own doc for the measurement behind it).
        // `!= Some("busy")` alone would have called this idle, which is
        // exactly the state a relay nudge must never type into.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        write_live_state_with_status(
            claude_home.path(),
            std::process::id(),
            "w1",
            Path::new("/work"),
            "waiting",
        );

        let observations = gather_relay_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            None,
            &app,
            None,
            &mut GlanceGate::default(),
        );
        let worker = observations.iter().find(|o| o.token == "worker-1").unwrap();
        assert_eq!(worker.is_idle_this_tick, Some(false));
    }

    #[test]
    fn a_claude_member_waiting_on_a_dialog_is_never_nudged_and_is_unknown_to_the_quiet_watch() {
        // `waitingFor` next to `waiting` (see `LIVE_WAITING_FOR_DIALOG_OPEN`): a
        // dialog is not idle for the relay, whose nudge would have its Enter choose
        // something, and is Unknown rather than not idle for the brigade watch;
        // every other reason, or none, is as it was.
        for (waiting_for, nudgeable, quiescence) in [
            (Some("dialog open"), Some(false), Quiescence::Unknown),
            (Some("input needed"), Some(false), Quiescence::NotIdle),
            (
                Some("a reason nobody has seen"),
                Some(false),
                Quiescence::NotIdle,
            ),
            (None, Some(false), Quiescence::NotIdle),
        ] {
            let claude_home = tempfile::tempdir().unwrap();
            let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
            let pid = std::process::id();
            let reason = waiting_for
                .map(|reason| format!(r#","waitingFor":"{reason}""#))
                .unwrap_or_default();
            let dir = claude_home.path().join("sessions");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{pid}.json")),
                format!(r#"{{"pid":{pid},"sessionId":"w1","status":"waiting"{reason}}}"#),
            )
            .unwrap();

            let mut gate = GlanceGate::default();
            let observations = gather_relay_observations(
                &state,
                &store,
                &ClaudeHome::new(claude_home.path().to_path_buf()),
                None,
                &app,
                None,
                &mut gate,
            );
            let worker = observations.iter().find(|o| o.token == "worker-1").unwrap();
            assert_eq!(worker.is_idle_this_tick, nudgeable, "{waiting_for:?}");
            let sampled = gate
                .quiescence
                .iter()
                .find(|(key, _)| key.as_str() == "w1")
                .map(|(_, sample)| *sample);
            assert_eq!(sampled, Some(quiescence), "{waiting_for:?}");
        }
    }

    #[test]
    fn a_claude_member_with_a_live_file_and_no_row_carries_its_live_status() {
        // A Worker that has not run a turn has a live file and no transcript, so
        // no session-list row; the product is the store's recorded choice. What
        // its file says is classified as the list does (shell is busy, a dialog
        // is still Waiting but told apart), and only for a Claude member. Only a
        // literal `busy` is also a running turn: `shell` classifies as busy and
        // is not one, which is what an interrupt must not press on.
        use banto_core::model::Activity::{Alive, Busy, Waiting};
        for (status, reason, expected) in [
            ("busy", None, Some((Busy, false, true))),
            ("waiting", None, Some((Waiting, false, false))),
            (
                "waiting",
                Some("input needed"),
                Some((Waiting, false, false)),
            ),
            ("waiting", Some("dialog open"), Some((Waiting, true, false))),
            ("shell", None, Some((Busy, false, false))),
            ("idle", None, Some((Alive, false, false))),
        ] {
            let claude_home = tempfile::tempdir().unwrap();
            let (store, state, app) = staged_worker("w1", None);
            let brigade_id = match &state.stage {
                Stage::Brigade { id, .. } => *id,
                other => panic!("{other:?}"),
            };
            store
                .borrow_mut()
                .set_member_agent(brigade_id, "worker-1", AgentKind::ClaudeCode)
                .unwrap();
            assert!(app.row_for_id("w1").is_none(), "no row");
            let dir = claude_home.path().join("sessions");
            std::fs::create_dir_all(&dir).unwrap();
            let pid = std::process::id();
            let reason = reason
                .map(|reason| format!(r#","waitingFor":"{reason}""#))
                .unwrap_or_default();
            std::fs::write(
                dir.join(format!("{pid}.json")),
                format!(r#"{{"pid":{pid},"sessionId":"w1","status":"{status}"{reason}}}"#),
            )
            .unwrap();
            let observations = gather_relay_observations(
                &state,
                &store,
                &ClaudeHome::new(claude_home.path().to_path_buf()),
                None,
                &app,
                None,
                &mut GlanceGate::default(),
            );
            let worker = observations.iter().find(|o| o.token == "worker-1").unwrap();
            assert_eq!(
                worker.live,
                expected.map(|(activity, dialog_open, status_busy)| LiveStatus {
                    activity,
                    dialog_open,
                    status_busy
                }),
                "{status} {reason}"
            );
        }
        // No live file: nothing to say.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", None);
        let brigade_id = match &state.stage {
            Stage::Brigade { id, .. } => *id,
            other => panic!("{other:?}"),
        };
        store
            .borrow_mut()
            .set_member_agent(brigade_id, "worker-1", AgentKind::ClaudeCode)
            .unwrap();
        let observations = gather_relay_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            None,
            &app,
            None,
            &mut GlanceGate::default(),
        );
        let worker = observations.iter().find(|o| o.token == "worker-1").unwrap();
        assert_eq!(worker.live, None);
    }

    #[test]
    fn a_claude_member_with_no_live_file_is_unknown_not_idle() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        // No sessions/<pid>.json written at all for "w1".

        let observations = gather_relay_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            None,
            &app,
            None,
            &mut GlanceGate::default(),
        );
        let worker = observations.iter().find(|o| o.token == "worker-1").unwrap();
        assert_eq!(worker.is_idle_this_tick, None);
    }

    #[test]
    fn a_member_whose_product_cannot_be_resolved_is_unknown_never_idle() {
        // The Worker has a live Claude-shaped session file AND a Codex
        // rollout mid-turn under the same id — an adversarial case showing
        // that without a matching `App` row, neither signal is trusted;
        // product must come from discovery, never be inferred from what
        // data exists.
        let claude_home = tempfile::tempdir().unwrap();
        let codex_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", None); // no row for "w1" at all
        write_live_state(
            claude_home.path(),
            std::process::id(),
            "w1",
            Path::new("/work"),
        );
        write_codex_rollout(codex_home.path(), "w1", &["task_started"]);

        let observations = gather_relay_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            Some(&CodexHome::new(codex_home.path().to_path_buf())),
            &app,
            None,
            &mut GlanceGate::default(),
        );
        let worker = observations.iter().find(|o| o.token == "worker-1").unwrap();
        assert_eq!(worker.is_idle_this_tick, None);
    }

    // --- model and effort only on creation: build_open_launch -------------

    struct MockProbe {
        alive: HashSet<u32>,
    }

    impl ProcessProbe for MockProbe {
        fn is_alive(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }

        fn parent_pid(&self, _pid: u32) -> Option<u32> {
            None
        }

        fn is_alive_matching(&self, pid: u32, _proc_start: &str) -> bool {
            self.is_alive(pid)
        }
    }

    fn open_target(id: &str) -> SessionToOpen {
        SessionToOpen {
            id: id.to_string(),
            agent: AgentKind::ClaudeCode,
            title: "Fix login".to_string(),
            cwd: PathBuf::from("/work/alpha"),
        }
    }

    /// An [`launch::OpenContext`] for tests that don't care about Codex
    /// liveness (`codex_home: None` degrades every Codex check to "not
    /// live" — see `codex_liveness::is_thread_alive`'s doc).
    fn test_ctx<'a>(
        probe: &'a dyn ProcessProbe,
        live: &'a [LiveSession],
    ) -> launch::OpenContext<'a> {
        launch::OpenContext {
            probe,
            live,
            codex_home: None,
            start_time: &SysinfoStartTime,
        }
    }

    #[test]
    fn build_open_launch_drops_a_model_and_an_effort_handed_to_a_resume_for_either_product() {
        // The rule lives at the seam as well as in the core's producers: a
        // producer that forgot it would still not reach the argv.
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let claude = build_open_launch(
            &open_target("sess-1"),
            Some("opus"),
            Some("max"),
            RoleFlags::default(),
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            claude.argv("claude"),
            ["claude", "--resume", "sess-1"].map(str::to_string)
        );

        let mut codex_target = open_target("codex-uuid-1");
        codex_target.agent = AgentKind::Codex;
        let codex = build_open_launch(
            &codex_target,
            Some("gpt-6.1-terra"),
            Some("high"),
            RoleFlags::default(),
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap()
        .argv("codex");
        assert_eq!(&codex[..3], ["codex", "resume", "codex-uuid-1"]);
        assert!(!codex.iter().any(|arg| arg == "-m"), "{codex:?}");
        assert!(
            !codex
                .iter()
                .any(|arg| arg.contains("model_reasoning_effort")),
            "{codex:?}"
        );
    }

    #[test]
    fn build_open_launch_gives_a_new_codex_session_its_model_and_effort() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let mut target = open_target("");
        target.agent = AgentKind::Codex;
        let argv = build_open_launch(
            &target,
            Some("gpt-6.1-terra"),
            Some("high"),
            RoleFlags::default(),
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap()
        .argv("codex");
        assert_eq!(&argv[..3], ["codex", "-m", "gpt-6.1-terra"]);
        assert!(argv.contains(&"model_reasoning_effort=\"high\"".to_string()));
        assert!(!argv.contains(&"resume".to_string()));
    }

    #[test]
    fn build_open_launch_appends_effort_right_after_model_and_omits_it_when_none() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let launch = build_open_launch(
            &open_target(""),
            Some("fable"),
            Some("max"),
            RoleFlags::default(),
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            launch.argv("claude"),
            ["claude", "--model", "fable", "--effort", "max"].map(str::to_string)
        );

        let no_effort = build_open_launch(
            &open_target(""),
            Some("fable"),
            None,
            RoleFlags::default(),
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            no_effort.argv("claude"),
            ["claude", "--model", "fable"].map(str::to_string)
        );
    }

    #[test]
    fn build_open_launch_appends_permission_mode_right_after_effort_and_omits_it_when_none() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let launch = build_open_launch(
            &open_target(""),
            Some("fable"),
            Some("max"),
            RoleFlags {
                permission_mode: Some("auto"),
                ..RoleFlags::default()
            },
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            launch.argv("claude"),
            [
                "claude",
                "--model",
                "fable",
                "--effort",
                "max",
                "--permission-mode",
                "auto"
            ]
            .map(str::to_string)
        );

        let no_permission_mode = build_open_launch(
            &open_target(""),
            Some("fable"),
            Some("max"),
            RoleFlags::default(),
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            no_permission_mode.argv("claude"),
            ["claude", "--model", "fable", "--effort", "max"].map(str::to_string)
        );
    }

    #[test]
    fn build_open_launch_appends_disallowed_tools_right_after_permission_mode_and_omits_it_when_none()
     {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let launch = build_open_launch(
            &open_target(""),
            Some("fable"),
            Some("max"),
            RoleFlags {
                permission_mode: Some("auto"),
                disallowed_tools: Some("Edit,Write,NotebookEdit"),
                ..RoleFlags::default()
            },
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            launch.argv("claude"),
            [
                "claude",
                "--model",
                "fable",
                "--effort",
                "max",
                "--permission-mode",
                "auto",
                "--disallowedTools",
                "Edit,Write,NotebookEdit"
            ]
            .map(str::to_string)
        );

        let no_disallowed_tools = build_open_launch(
            &open_target(""),
            Some("fable"),
            Some("max"),
            RoleFlags {
                permission_mode: Some("auto"),
                ..RoleFlags::default()
            },
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            no_disallowed_tools.argv("claude"),
            [
                "claude",
                "--model",
                "fable",
                "--effort",
                "max",
                "--permission-mode",
                "auto"
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn build_open_launch_appends_model_to_a_fresh_launchs_argv_too() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let launch = build_open_launch(
            &open_target(""),
            Some("opus"),
            None,
            RoleFlags::default(),
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            launch.argv("claude"),
            ["claude", "--model", "opus"].map(str::to_string)
        );
    }

    #[test]
    fn build_open_launch_omits_model_when_none() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let launch = build_open_launch(
            &open_target("sess-1"),
            None,
            None,
            RoleFlags::default(),
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            launch.argv("claude"),
            ["claude", "--resume", "sess-1"].map(str::to_string)
        );
    }

    #[test]
    fn build_open_launch_is_none_when_the_resume_is_refused() {
        let probe = MockProbe {
            alive: HashSet::from([4242]),
        };
        let live = [LiveSession {
            pid: 4242,
            session_id: Some("sess-1".to_string()),
            cwd: None,
            status: None,
            waiting_for: None,
            kind: None,
            name: None,
            proc_start: None,
            version: None,
        }];
        assert!(
            build_open_launch(
                &open_target("sess-1"),
                Some("opus"),
                None,
                RoleFlags::default(),
                None,
                &test_ctx(&probe, &live),
            )
            .is_none()
        );
    }

    #[test]
    fn build_open_launch_ignores_a_live_session_that_is_a_different_one() {
        // The guard refuses a resume of the session that is running, not any
        // resume while something is.
        let probe = MockProbe {
            alive: HashSet::from([4242]),
        };
        let live = [LiveSession {
            pid: 4242,
            session_id: Some("some-other-session".to_string()),
            cwd: None,
            status: None,
            waiting_for: None,
            kind: None,
            name: None,
            proc_start: None,
            version: None,
        }];
        assert!(
            build_open_launch(
                &open_target("sess-1"),
                None,
                None,
                RoleFlags::default(),
                None,
                &test_ctx(&probe, &live),
            )
            .is_some()
        );
    }

    fn live_entry(pid: u32, cwd: Option<&Path>) -> LiveSession {
        LiveSession {
            pid,
            session_id: Some("sess-live".to_string()),
            cwd: cwd.map(Path::to_path_buf),
            status: None,
            waiting_for: None,
            kind: None,
            name: None,
            proc_start: None,
            version: None,
        }
    }

    /// Creates a directory symlink, or says why it could not and returns
    /// false: the test then skips, unless `BANTO_TEST_ALLOW_SKIPS` is unset, in
    /// which case a silent pass would hide the gap and it fails instead. The
    /// same convention as `banto_io::file_identity`'s tests, whose helper is
    /// `cfg(test)` of another crate and so out of reach here.
    fn try_symlink_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(target, link);
        match made {
            Ok(()) => true,
            Err(error) => {
                assert!(
                    std::env::var_os("BANTO_TEST_ALLOW_SKIPS").is_some_and(|v| v == "1"),
                    "cannot create a directory symlink here ({error}); set \
                     BANTO_TEST_ALLOW_SKIPS=1 to let that pass"
                );
                eprintln!("skipped: cannot create a directory symlink here ({error})");
                false
            }
        }
    }

    /// Claude reports the directory it resolved; banto spawned through a
    /// symlink, so the two spellings differ while the directory is one.
    #[test]
    fn live_session_id_matches_a_cwd_reached_through_a_symlink() {
        let work = tempfile::TempDir::new().unwrap();
        let real = work.path().join("real");
        let link = work.path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(work.path().join("elsewhere")).unwrap();
        if !try_symlink_dir(&real, &link) {
            return;
        }
        let live = [live_entry(4242, Some(&real.canonicalize().unwrap()))];

        assert_eq!(
            live_session_id(&live, 4242, &DirectorySpellings::of(&link)),
            Some("sess-live".to_string())
        );
        assert_eq!(
            live_session_id(
                &live,
                4242,
                &DirectorySpellings::of(&work.path().join("elsewhere"))
            ),
            None
        );
    }

    #[test]
    fn live_session_id_declines_another_pid_a_missing_cwd_and_a_vanished_directory() {
        let work = tempfile::TempDir::new().unwrap();
        let dir = work.path().to_path_buf();
        let spawned = DirectorySpellings::of(&dir);
        assert_eq!(
            live_session_id(&[live_entry(1, Some(&dir))], 2, &spawned),
            None,
            "another process's file"
        );
        assert_eq!(
            live_session_id(&[live_entry(1, None)], 1, &spawned),
            None,
            "a file with no cwd"
        );
        let gone = work.path().join("gone");
        assert_eq!(
            live_session_id(
                &[live_entry(1, Some(&dir))],
                1,
                &DirectorySpellings::of(&gone)
            ),
            None,
            "a spawn directory that no longer exists"
        );
        assert_eq!(
            live_session_id(
                &[live_entry(1, Some(&gone))],
                1,
                &DirectorySpellings::of(&gone)
            ),
            Some("sess-live".to_string()),
            "equal spellings need no directory on disk"
        );
    }

    #[test]
    fn build_open_launch_codex_target_resumes_via_codex_with_cwd() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let mut target = open_target("codex-uuid-1");
        target.agent = AgentKind::Codex;
        let launch = build_open_launch(
            &target,
            // A resume carries no model whatever the caller hands it — see
            // `build_open_launch`'s doc.
            Some("o3"),
            // `effort` is dropped on a resume too, and the role flags are
            // Claude-only (`AgentLaunch::Claude`'s own doc); dropped here for
            // the same reason the briefing below is.
            None,
            RoleFlags::default(),
            // The caller resolves a briefing for any brigade member
            // regardless of product (`member_briefing` takes only
            // `brigade_id`/`token`/`role`, never `agent` — see
            // `execute_open_embedded`'s own call) and passes it through
            // uniformly; Codex has no `--append-system-prompt` equivalent
            // to put it in, so it's dropped here on purpose. A Codex
            // member's own briefing arrives a different way entirely — the
            // `SessionStart` hook's `additionalContext` (`crate::hook`'s
            // module doc), never through argv at all.
            Some("you are the Director"),
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            launch.argv("codex"),
            [
                "codex",
                "resume",
                "codex-uuid-1",
                "-c",
                "tui.notifications=[\"approval-requested\",\"async-question\",\"plan-mode-prompt\"]",
                "-c",
                "tui.notification_method=\"bel\"",
                "-c",
                "tui.notification_condition=\"always\"",
                "-C",
                "/work/alpha",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn build_open_launch_drops_permission_mode_for_a_codex_target_even_when_some() {
        // Unlike the Goinkyo (always Claude), a Worker can resolve to either
        // product — `worker_permission_mode` reaches this function as `Some`
        // for a Codex Worker too (`engine::RoleLaunch::of` doesn't gate it by
        // agent), so this is no longer only ever structurally `None` here by
        // construction. `AgentLaunch::Codex` still has no field for it.
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let mut target = open_target("codex-uuid-1");
        target.agent = AgentKind::Codex;
        let launch = build_open_launch(
            &target,
            Some("o3"),
            None,
            RoleFlags {
                permission_mode: Some("auto"),
                ..RoleFlags::default()
            },
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert!(
            !launch
                .argv("codex")
                .contains(&"--permission-mode".to_string()),
            "Codex has no --permission-mode equivalent to carry this into"
        );
    }

    // --- role briefings (--append-system-prompt) -------------------------

    #[test]
    fn build_open_launch_appends_the_briefing_after_the_model_of_a_new_session_and_after_resume_on_a_resume()
     {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let created = build_open_launch(
            &open_target(""),
            Some("opus"),
            None,
            RoleFlags::default(),
            Some("you are the Director"),
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            created.argv("claude"),
            [
                "claude",
                "--model",
                "opus",
                "--append-system-prompt",
                "you are the Director",
            ]
            .map(str::to_string)
        );
        // The briefing is the role's, so it rides on a resume — unlike the model.
        let resumed = build_open_launch(
            &open_target("sess-1"),
            Some("opus"),
            None,
            RoleFlags::default(),
            Some("you are the Director"),
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        assert_eq!(
            resumed.argv("claude"),
            [
                "claude",
                "--resume",
                "sess-1",
                "--append-system-prompt",
                "you are the Director",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn build_open_launch_leaves_mcp_config_for_the_caller_to_fill_in() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let launch = build_open_launch(
            &open_target("sess-1"),
            None,
            None,
            RoleFlags::default(),
            None,
            &test_ctx(&probe, &[]),
        )
        .unwrap();
        match launch {
            launch::AgentLaunch::Claude { mcp_config, .. } => assert_eq!(mcp_config, None),
            launch::AgentLaunch::Codex { .. } => panic!("expected a Claude launch"),
        }
    }

    #[test]
    fn brigade_env_carries_the_identity_the_hook_command_must_not() {
        assert_eq!(
            brigade_env(
                Some(&(7, "worker-1".to_string(), BrigadeRole::Worker)),
                None
            ),
            vec![
                ("BANTO_BRIGADE".to_string(), "7".to_string()),
                ("BANTO_MEMBER".to_string(), "worker-1".to_string()),
                ("BANTO_ROLE".to_string(), "worker".to_string()),
            ]
        );
    }

    #[test]
    fn brigade_env_is_empty_outside_a_brigade() {
        let parent = crate::startup::Resolved {
            config_file: crate::startup::ConfigFile::None,
            db_path: None,
            claude_home: None,
            codex_home: None,
            data_dir: None,
        };
        assert!(brigade_env(None, None).is_empty());
        // Nothing to say to a process that is not a brigade member's.
        assert!(brigade_env(None, Some(&parent)).is_empty());
    }

    #[test]
    fn a_members_environment_also_names_the_config_and_database_its_parent_resolved() {
        let parent = crate::startup::Resolved {
            config_file: crate::startup::ConfigFile::File(PathBuf::from("/c dir/banto.toml")),
            db_path: Some(PathBuf::from("/d dir/banto.db")),
            claude_home: Some(PathBuf::from("/h/claude")),
            codex_home: Some(PathBuf::from("/h/codex")),
            data_dir: None,
        };
        let env = brigade_env(
            Some(&(7, "worker-1".to_string(), BrigadeRole::Worker)),
            Some(&parent),
        );
        assert_eq!(env.len(), 5, "{env:?}");
        assert!(env.contains(&(
            "BANTO_PARENT_CONFIG".to_string(),
            "/c dir/banto.toml".to_string()
        )));
        assert!(env.contains(&("BANTO_PARENT_DB".to_string(), "/d dir/banto.db".to_string())));
        // Banto's own variables only: an agent's home is the agent's to decide.
        assert!(env.iter().all(|(name, _)| name.starts_with("BANTO_")));
    }

    #[test]
    fn the_claude_mcp_config_carries_the_parents_resolution_between_the_subcommand_and_the_identity()
     {
        let json = mcp_config_json(
            Path::new("/opt/banto/banto"),
            &[
                "--parent-no-config".to_string(),
                "--db-path".to_string(),
                "/d dir/banto.db".to_string(),
            ],
            7,
            "worker-1",
            BrigadeRole::Worker,
            Some("sess-1"),
        );
        let args: Vec<&str> = json["mcpServers"]["banto"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "_mcp",
                "--parent-no-config",
                "--db-path",
                "/d dir/banto.db",
                "--brigade",
                "7",
                "--member",
                "worker-1",
                "--role",
                "worker",
                "--session",
                "sess-1"
            ]
        );
    }

    #[test]
    fn member_briefing_names_the_opposite_role_as_peers_and_honors_the_empty_escape_hatch() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let brigade_id = {
            let mut store = store.borrow_mut();
            let id = store.create_brigade("cell").unwrap();
            store
                .add_brigade_member(id, "director", BrigadeRole::Director, None)
                .unwrap();
            store
                .add_brigade_member(id, "worker-1", BrigadeRole::Worker, None)
                .unwrap();
            store
                .add_brigade_member(id, "worker-2", BrigadeRole::Worker, None)
                .unwrap();
            id
        };
        let meta_cache = MetaCache::new();
        let pty_host = MockPtyHost::default();
        let input_reader = MockInputReader::default();
        let thresholds = AgeThresholds::default();
        let config = BrigadeConfig {
            director_prompt: "I am {token}; my team is {peers}".to_string(),
            worker_prompt: "I am {token}; I report to {peers}".to_string(),
            ..BrigadeConfig::default()
        };
        let silent = BrigadeConfig {
            director_prompt: String::new(),
            ..config.clone()
        };
        let claude_home = ClaudeHome::new(PathBuf::from("/nonexistent"));
        let agent_binaries = AgentBinaries::default();
        let enabled_agents = all_agents();
        let deps = |brigade| Deps {
            claude_home: &claude_home,
            codex_home: None,
            thresholds: &thresholds,
            store: &store,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
            codex_catalog: &NoCatalog,
            child: None,
            exe: crate::exe::test_support::current(),
        };

        assert_eq!(
            member_briefing(
                &deps(&config),
                brigade_id,
                "director",
                BrigadeRole::Director
            ),
            Some(format!(
                "I am director; my team is worker-1, worker-2

{}",
                crate::briefing::consult_paragraph(banto_core::config::GoinkyoConsult::Design)
            )),
            "a Director's template is followed by the operator's consultation level"
        );
        assert_eq!(
            member_briefing(&deps(&config), brigade_id, "worker-1", BrigadeRole::Worker),
            Some("I am worker-1; I report to director".to_string()),
            "a Worker's addressable peer is the Director, not its siblings"
        );
        assert_eq!(
            member_briefing(
                &deps(&silent),
                brigade_id,
                "director",
                BrigadeRole::Director
            ),
            None,
            "an empty template launches with no flag at all"
        );
    }

    #[test]
    fn member_briefing_substitutes_request_for_a_goinkyo_and_leaves_it_alone_for_a_director() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let brigade_id = {
            let mut store = store.borrow_mut();
            let id = store.create_brigade("cell").unwrap();
            store
                .add_brigade_member(id, "director", BrigadeRole::Director, None)
                .unwrap();
            store
                .add_brigade_member(id, "goinkyo", BrigadeRole::Goinkyo, None)
                .unwrap();
            id
        };
        let meta_cache = MetaCache::new();
        let pty_host = MockPtyHost::default();
        let input_reader = MockInputReader::default();
        let thresholds = AgeThresholds::default();
        let config = BrigadeConfig {
            // The Director's own template happens to contain the literal
            // text `{request}` too, to prove it is left untouched for a
            // role `render` was not given a request path for.
            director_prompt: "director sees: {request}".to_string(),
            goinkyo_prompt: "read {request} first".to_string(),
            // No level paragraph, so the Director's text is exactly its template.
            goinkyo_consult: String::new(),
            ..BrigadeConfig::default()
        };
        let claude_home = ClaudeHome::new(PathBuf::from("/nonexistent"));
        let agent_binaries = AgentBinaries::default();
        let enabled_agents = all_agents();
        let deps = Deps {
            claude_home: &claude_home,
            codex_home: None,
            thresholds: &thresholds,
            store: &store,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade: &config,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
            codex_catalog: &NoCatalog,
            child: None,
            exe: crate::exe::test_support::current(),
        };

        let goinkyo_briefing = member_briefing(&deps, brigade_id, "goinkyo", BrigadeRole::Goinkyo)
            .expect("goinkyo_prompt is non-empty");
        assert!(
            goinkyo_briefing.starts_with("read ")
                && goinkyo_briefing.ends_with(&format!("{brigade_id}.txt first")),
            "expected the request path spliced into the template, got {goinkyo_briefing:?}"
        );
        assert!(
            !goinkyo_briefing.contains("{request}"),
            "got {goinkyo_briefing:?}"
        );

        let director_briefing =
            member_briefing(&deps, brigade_id, "director", BrigadeRole::Director)
                .expect("director_prompt is non-empty");
        assert_eq!(
            director_briefing, "director sees: {request}",
            "a Director's template must not have {{request}} substituted"
        );
    }

    #[test]
    fn goinkyo_request_path_matches_what_the_mcp_server_writes_to() {
        // The `_mcp` process is handed `DataDir::goinkyo_dir` and writes
        // `mcp::goinkyo_request_path(that, brigade)`; the briefing's `{request}` is
        // computed here from the same `DataDir`. They must name one file.
        let data_dir = DataDir::new(PathBuf::from("/a data dir"));
        let written_by_mcp = crate::mcp::goinkyo_request_path(&data_dir.goinkyo_dir(), 42)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(goinkyo_request_path(&data_dir, 42), Some(written_by_mcp));
    }

    // --- gather_goinkyo_observations -----------------------------------------

    /// A staged brigade of a hosted Director plus a Goinkyo member row with no
    /// session id yet — the shape `gather_goinkyo_observations` reads.
    /// `goinkyo_session_id`, when given, is written to the member row
    /// instead, for the "already resolved" case.
    fn staged_goinkyo(goinkyo_session_id: Option<&str>) -> (RefCell<Store>, EmporiumState, App) {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "director",
                BrigadeRole::Director,
                Some(&SessionId("dir".to_string())),
            )
            .unwrap();
        let session = goinkyo_session_id.map(|id| SessionId(id.to_string()));
        store
            .add_brigade_member(
                brigade_id,
                "goinkyo",
                BrigadeRole::Goinkyo,
                session.as_ref(),
            )
            .unwrap();

        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Brigade {
            id: brigade_id,
            director: Some(SessionKey::from_id("dir")),
            panes: vec![SessionKey::from_id("dir")],
            focused: 0,
        };
        // The Director's pane is hosted: the relay's notion of a live brigade.
        state
            .screens
            .insert(SessionKey::from_id("dir"), Screen::new(24, 80));
        let mut dir_row = test_row("dir", AgentKind::ClaudeCode);
        dir_row.cwd = Some(PathBuf::from("/work/alpha"));
        let app = App::new(vec![dir_row]);
        (RefCell::new(store), state, app)
    }

    /// The only observation `gather_goinkyo_observations` made, or
    /// `Unchanged` when it made none. The gatherer itself never emits
    /// `Unchanged` (asserted here), so a test comparing with `Unchanged`
    /// is asserting that nothing was observed.
    fn only_observation(
        state: &EmporiumState,
        store: &RefCell<Store>,
        app: &App,
    ) -> GoinkyoObservation {
        let mut observations = gather_goinkyo_observations(state, store, app, None);
        assert!(observations.len() <= 1, "{observations:?}");
        assert!(
            !observations
                .iter()
                .any(|o| matches!(o, GoinkyoObservation::Unchanged)),
            "{observations:?}"
        );
        observations.pop().unwrap_or(GoinkyoObservation::Unchanged)
    }

    #[test]
    fn the_spawn_candidate_carries_the_roster_in_the_order_the_members_joined() {
        let (store, state, app) = staged_goinkyo(None);
        // A Worker joins after the consultation: by token it would sort after
        // `goinkyo` too, so give it a token that sorts before it.
        store
            .borrow_mut()
            .add_brigade_member(1, "assistant", BrigadeRole::Worker, None)
            .unwrap();

        let GoinkyoObservation::AwaitingSpawn(candidate) = only_observation(&state, &store, &app)
        else {
            panic!("a Goinkyo row with no session is awaiting a spawn");
        };
        assert_eq!(candidate.roster, ["director", "goinkyo", "assistant"]);
    }

    #[test]
    fn a_consultation_that_ends_while_another_brigade_is_on_stage_is_still_noticed() {
        // `dismiss_goinkyo` is an MCP call the Director's own session makes,
        // so it lands whenever that session decides to — not when the
        // operator happens to be looking at the brigade it belongs to. Tied
        // to the stage, this observation would never fire for those, and the
        // pane would keep running with no row behind it and nothing left
        // that knew which pane it was.
        let (store, mut state, app) = staged_goinkyo(None);
        let mut app = app;
        let brigade_id = store.borrow().list_brigades().unwrap()[0].id;
        let config = BrigadeConfig::default();

        // Record the pane the ordinary way: gather says "awaiting spawn",
        // the fold opens it and remembers which brigade it belongs to.
        let observation = only_observation(&state, &store, &app);
        assert!(matches!(observation, GoinkyoObservation::AwaitingSpawn(_)));
        engine::update(
            &mut state,
            &mut app,
            &config,
            Event::GoinkyoAwaitingSpawn { observation },
            Instant::now(),
        );

        // The consultation ends, and the operator is looking elsewhere.
        store
            .borrow_mut()
            .dismiss_worker(brigade_id, "goinkyo")
            .unwrap();
        state.stage = Stage::Empty;

        assert!(
            matches!(
                only_observation(&state, &store, &app),
                GoinkyoObservation::NoGoinkyo { brigade_id: id } if id == brigade_id
            ),
            "the ended consultation must be reported whatever is on stage"
        );
    }

    #[test]
    fn a_brigade_nobody_is_running_is_not_asked_about() {
        // Neither a Goinkyo pane recorded nor a hosted Director: a brigade
        // that is not in use must not produce an observation just because it
        // has no Goinkyo row.
        let (store, mut state, app) = staged_goinkyo(None);
        let brigade_id = store.borrow().list_brigades().unwrap()[0].id;
        store
            .borrow_mut()
            .dismiss_worker(brigade_id, "goinkyo")
            .unwrap();
        state.stage = Stage::Empty;
        state.screens.clear();

        assert!(matches!(
            only_observation(&state, &store, &app),
            GoinkyoObservation::Unchanged
        ));
    }

    #[test]
    fn a_goinkyo_row_with_no_session_id_is_a_spawn_candidate_in_the_directors_cwd() {
        let (store, state, app) = staged_goinkyo(None);
        let GoinkyoObservation::AwaitingSpawn(candidate) = only_observation(&state, &store, &app)
        else {
            panic!("a Goinkyo row with no session id must be reported as awaiting spawn");
        };
        assert_eq!(candidate.cwd, PathBuf::from("/work/alpha"));
    }

    #[test]
    fn a_goinkyo_row_with_no_session_id_is_a_spawn_candidate_in_the_directors_cwd_even_when_panes_are_reversed()
     {
        // The Worker staged solo first, ahead of the Director, so
        // `panes[0]` is the Worker — proves the cwd lookup reads
        // `director`, not position (the Worker's own row is a distinct,
        // deliberately different cwd to catch a positional read).
        let (store, mut state, _app) = staged_goinkyo(None);
        let worker = SessionKey::from_id("w1");
        if let Stage::Brigade { panes, .. } = &mut state.stage {
            panes.insert(0, worker.clone());
        }
        let mut dir_row = test_row("dir", AgentKind::ClaudeCode);
        dir_row.cwd = Some(PathBuf::from("/work/alpha"));
        let mut worker_row = test_row("w1", AgentKind::ClaudeCode);
        worker_row.cwd = Some(PathBuf::from("/work/wrong"));
        let app = App::new(vec![worker_row, dir_row]);

        let GoinkyoObservation::AwaitingSpawn(candidate) = only_observation(&state, &store, &app)
        else {
            panic!("a Goinkyo row with no session id must be reported as awaiting spawn");
        };
        assert_eq!(candidate.cwd, PathBuf::from("/work/alpha"));
    }

    #[test]
    fn a_goinkyo_row_already_holding_a_session_id_is_unchanged() {
        // A session id means discovery (or a resume) already resolved this
        // Goinkyo to a pane — nothing left for the tick to spawn, and this
        // is not the "row is gone" case either, so the guard must not be
        // released.
        let (store, state, app) = staged_goinkyo(Some("g1"));
        assert_eq!(
            only_observation(&state, &store, &app),
            GoinkyoObservation::Unchanged
        );
    }

    #[test]
    fn no_goinkyo_member_at_all_reports_the_row_as_gone() {
        // Distinct from every other "nothing to report" case: this is what
        // tells `update_goinkyo_awaiting_spawn` a consultation was
        // dismissed out from under a live brigade, so it can release
        // `goinkyo_pane` for a later one.
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "director",
                BrigadeRole::Director,
                Some(&SessionId("dir".to_string())),
            )
            .unwrap();
        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Brigade {
            id: brigade_id,
            director: Some(SessionKey::from_id("dir")),
            panes: vec![SessionKey::from_id("dir")],
            focused: 0,
        };
        state
            .screens
            .insert(SessionKey::from_id("dir"), Screen::new(24, 80));
        let app = App::new(vec![test_row("dir", AgentKind::ClaudeCode)]);

        assert_eq!(
            only_observation(&state, &RefCell::new(store), &app),
            GoinkyoObservation::NoGoinkyo { brigade_id }
        );
    }

    #[test]
    fn with_no_brigade_at_all_there_is_nothing_to_report() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let state = EmporiumState::new(PrefixKey::default());
        let app = App::new(vec![]);
        assert_eq!(
            only_observation(&state, &store, &app),
            GoinkyoObservation::Unchanged
        );
    }

    #[test]
    fn a_brigade_off_stage_with_a_hosted_director_is_a_spawn_candidate() {
        // The Director consults the Goinkyo while the operator looks at another
        // brigade, or at none: the spawn does not wait for the stage.
        let (store, mut state, app) = staged_goinkyo(None);
        state.stage = Stage::Empty;

        let GoinkyoObservation::AwaitingSpawn(candidate) = only_observation(&state, &store, &app)
        else {
            panic!("a hosted Director's brigade is live whatever is on stage");
        };
        assert_eq!(candidate.cwd, PathBuf::from("/work/alpha"));
        assert_eq!(candidate.roster, ["director", "goinkyo"]);
    }

    #[test]
    fn a_brigade_whose_director_is_not_hosted_is_left_alone() {
        // A paneless Goinkyo row can outlive the run that wrote it: it must not
        // start at banto's startup for a Director that is not running.
        let (store, mut state, app) = staged_goinkyo(None);
        state.stage = Stage::Empty;
        state.screens.clear();

        assert_eq!(
            gather_goinkyo_observations(&state, &store, &app, None),
            Vec::new()
        );
    }

    #[test]
    fn a_director_row_that_resolves_to_no_session_row_gives_the_current_directory() {
        let (store, state, _app) = staged_goinkyo(None);
        let app = App::new(vec![]);

        let GoinkyoObservation::AwaitingSpawn(candidate) = only_observation(&state, &store, &app)
        else {
            panic!("the Director is hosted");
        };
        assert_eq!(candidate.cwd, PathBuf::from("."));
    }

    #[test]
    fn every_live_brigade_is_observed_in_the_same_tick() {
        // One standing candidate must not hide the next brigade's.
        let (store, mut state, _app) = staged_goinkyo(None);
        let second = {
            let mut store = store.borrow_mut();
            let id = store.create_brigade("second").unwrap();
            store
                .add_brigade_member(
                    id,
                    "director",
                    BrigadeRole::Director,
                    Some(&SessionId("dir2".to_string())),
                )
                .unwrap();
            store
                .add_brigade_member(id, "goinkyo", BrigadeRole::Goinkyo, None)
                .unwrap();
            id
        };
        state.stage = Stage::Empty;
        state
            .screens
            .insert(SessionKey::from_id("dir2"), Screen::new(24, 80));
        let mut first_row = test_row("dir", AgentKind::ClaudeCode);
        first_row.cwd = Some(PathBuf::from("/work/alpha"));
        let mut second_row = test_row("dir2", AgentKind::ClaudeCode);
        second_row.cwd = Some(PathBuf::from("/work/beta"));
        let app = App::new(vec![first_row, second_row]);

        let observations = gather_goinkyo_observations(&state, &store, &app, None);
        let candidates: Vec<(BrigadeId, PathBuf)> = observations
            .into_iter()
            .map(|observation| match observation {
                GoinkyoObservation::AwaitingSpawn(candidate) => {
                    (candidate.brigade_id, candidate.cwd)
                }
                other => panic!("expected candidates only, got {other:?}"),
            })
            .collect();
        assert_eq!(
            candidates,
            [
                (1, PathBuf::from("/work/alpha")),
                (second, PathBuf::from("/work/beta"))
            ]
        );
    }

    #[test]
    fn a_disbanded_brigades_recorded_goinkyo_is_reported_gone() {
        // The guard of a brigade that no longer exists is released by the
        // gather's own pass over the brigades it has a pane recorded for: its
        // roster reads as empty, not as an error, and its Director's pane is
        // gone with it.
        let (store, mut state, mut app) = staged_goinkyo(None);
        let brigade_id = store.borrow().list_brigades().unwrap()[0].id;
        let observation = only_observation(&state, &store, &app);
        engine::update(
            &mut state,
            &mut app,
            &BrigadeConfig::default(),
            Event::GoinkyoAwaitingSpawn { observation },
            Instant::now(),
        );
        assert_eq!(state.goinkyo_brigades(), [brigade_id]);

        store.borrow_mut().delete_brigade(brigade_id).unwrap();
        state.stage = Stage::Empty;
        state.screens.clear();

        assert_eq!(
            only_observation(&state, &store, &app),
            GoinkyoObservation::NoGoinkyo { brigade_id }
        );
    }

    #[test]
    fn a_stranded_goinkyos_session_clears_for_real_and_respawns_exactly_once() {
        // The other half of the `stage_brigade` fix for a stranded Goinkyo,
        // exercised through the real store round trip `engine.rs`'s own
        // pure unit tests can't reach: does clearing `session_id` actually
        // flip `gather_goinkyo_observations` back to `AwaitingSpawn`, and
        // does the existing one-shot guard (`EmporiumState::goinkyo_pane`)
        // still stop it from spawning a second time once it has.
        let (store, mut state, app) = staged_goinkyo(Some("g1"));
        let Stage::Brigade { id: brigade_id, .. } = state.stage else {
            panic!("staged_goinkyo always stages a Stage::Brigade");
        };

        // Simulates `stage_brigade`'s own `Cmd::Store(ClearMemberSession)`
        // actually executing — proven separately, at the pure-logic level,
        // by `engine.rs`'s own
        // `stage_brigade_resets_a_stranded_goinkyos_session_id_instead_of_
        // reporting_it_missing`.
        let events = execute_store_intent(
            StoreIntent::ClearMemberSession {
                brigade_id,
                token: "goinkyo".to_string(),
            },
            &store,
        );
        assert!(matches!(
            events.as_slice(),
            [Event::MemberSessionRecorded { .. }]
        ));

        // Next tick: the same consultation is spawnable again.
        let observation = only_observation(&state, &store, &app);
        let GoinkyoObservation::AwaitingSpawn(candidate) = &observation else {
            panic!("expected AwaitingSpawn once session_id cleared, got {observation:?}");
        };
        assert_eq!(candidate.brigade_id, brigade_id);

        let brigade = BrigadeConfig::default();
        let cmds = engine::update(
            &mut state,
            &mut App::new(vec![]),
            &brigade,
            Event::GoinkyoAwaitingSpawn { observation },
            Instant::now(),
        );
        assert_eq!(
            cmds.iter()
                .filter(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. }))
                .count(),
            1,
            "expected exactly one respawn: {cmds:?}"
        );

        // A further tick before the respawned pane even reports back
        // (`Event::Spawned`, which is what actually records a session id
        // and would make `gather_goinkyo_observations` itself report
        // `Unchanged`) must not spawn a second one. The store still shows
        // `session_id: None` at this point — the shell's own observation is
        // correctly still `AwaitingSpawn`, same as just above; it's
        // `update_goinkyo_awaiting_spawn`'s own one-shot guard
        // (`EmporiumState::goinkyo_pane`, armed by the first spawn above)
        // that has to be the thing stopping a second one — the guard this
        // whole fix has to respect, not bypass.
        let observation_again = only_observation(&state, &store, &app);
        assert!(
            matches!(observation_again, GoinkyoObservation::AwaitingSpawn(_)),
            "the store genuinely hasn't changed yet, so the shell's own \
             observation is unchanged too: {observation_again:?}"
        );
        let cmds_again = engine::update(
            &mut state,
            &mut App::new(vec![]),
            &brigade,
            Event::GoinkyoAwaitingSpawn {
                observation: observation_again,
            },
            Instant::now(),
        );
        assert!(
            !cmds_again
                .iter()
                .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. })),
            "the one-shot spawn guard must stop a second respawn: {cmds_again:?}"
        );
    }

    #[test]
    fn a_goinkyo_discovered_in_the_pass_that_observes_it_is_not_torn_down_as_stale() {
        // The window a live run fell into twice: the hook records the Goinkyo's
        // briefing, and `poll_discovery` finds it in the very pass that is also
        // the tick pass. The pane is rekeyed to its real id and the member row
        // is written when that pass's events are handled; an observation read
        // before that sees a row with no session beside a pane that already has
        // a real key, which is what a pane left over from an ended consultation
        // looks like.
        let (store, mut state, mut app) = staged_goinkyo(None);
        let brigade = BrigadeConfig::default();
        let synthetic = pending_key("goinkyo");
        let observation = only_observation(&state, &store, &app);
        let cmds = engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoAwaitingSpawn { observation },
            Instant::now(),
        );
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { key, .. } if *key == synthetic)),
            "{cmds:?}"
        );
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: synthetic.clone(),
                agent: None,
            },
            Instant::now(),
        );
        let spawned = SystemTime::now() - Duration::from_secs(5);
        store
            .borrow()
            .record_briefing(
                1,
                "goinkyo",
                &SessionId("g1".to_string()),
                SystemTime::now(),
            )
            .unwrap();

        let claude_home = tempfile::tempdir().unwrap();
        let claude_home_ref = ClaudeHome::new(claude_home.path().to_path_buf());
        let provider = ClaudeCodeProvider::new(claude_home_ref.clone());
        let mut trackers = vec![codex_tracker(1, "goinkyo", spawned)];

        // The pass, as the loop runs it: discovery is polled, the tick is due,
        // and the events are handled by `next_event`, which is what reads the
        // Goinkyo.
        let mut events: VecDeque<Event> = poll_discovery(
            &mut trackers,
            &provider,
            &HashSet::new(),
            &[],
            &store,
            &claude_home_ref,
        )
        .into();
        assert!(
            matches!(events.front(), Some(Event::DiscoveryResult { session_id, .. }) if session_id == "g1"),
            "{events:?}"
        );
        let mut goinkyo_owed = true;

        let mut issued = Vec::new();
        while let Some(event) = next_event(&mut events, &mut goinkyo_owed, || {
            goinkyo_events(&state, &store, &app, None, None)
        }) {
            for cmd in engine::update(&mut state, &mut app, &brigade, event, Instant::now()) {
                if let Cmd::Store(intent) = &cmd {
                    events.extend(execute_store_intent(intent.clone(), &store));
                }
                issued.push(cmd);
            }
        }

        assert!(
            !issued
                .iter()
                .any(|cmd| matches!(cmd, Cmd::HangUpPty { .. })),
            "a Goinkyo that was just identified must keep its pane: {issued:?}"
        );
        assert_eq!(
            state.goinkyo_guard_facts(1).guard,
            Some(SessionKey::from_id("g1")),
            "the guard must still name the live pane"
        );
    }

    #[test]
    fn owed_observations_are_read_only_when_the_queue_is_empty_and_once() {
        let mut queue: VecDeque<Event> = VecDeque::from([
            Event::Resized {
                width: 80,
                height: 24,
            },
            Event::Resized {
                width: 90,
                height: 30,
            },
        ]);
        let mut owed = true;
        let mut reads = 0;
        let mut handled = Vec::new();
        while let Some(event) = next_event(&mut queue, &mut owed, || {
            reads += 1;
            // What the read sees: everything queued before it has been handled.
            assert_eq!(handled.len(), 2);
            vec![Event::Tick {
                relay: Vec::new(),
                brigades: Vec::new(),
            }]
        }) {
            handled.push(event);
        }
        assert_eq!(reads, 1);
        assert_eq!(handled.len(), 3);
        assert!(matches!(handled[2], Event::Tick { .. }));

        // Not owed: never read.
        let mut owed = false;
        assert!(next_event(&mut queue, &mut owed, || panic!("not owed")).is_none());
    }

    // --- tile_label -----------------------------------------------------

    #[test]
    fn tile_label_uses_member_token_even_when_panes_are_reversed() {
        // Drive formation through the core so this test proves the renderer
        // receives the token as state data, then reverse geometry to prove
        // neither label is derived from a pane index.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = App::new(vec![test_row("dir", AgentKind::ClaudeCode)]);
        let brigade = BrigadeConfig::default();
        let now = Instant::now();
        let director_cmds = engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::BrigadeFormed {
                director_row_id: "dir".to_string(),
                name: "cell".to_string(),
                cwd: PathBuf::from("/work"),
                worker_agent: AgentKind::ClaudeCode,
                worker_model: "sonnet".to_string(),
                worker_effort: "high".to_string(),
                result: Ok((1, vec!["worker-1".to_string()])),
            },
            now,
        );
        let director = match director_cmds.as_slice() {
            [Cmd::OpenEmbedded { key, .. }] => key.clone(),
            other => panic!("expected a Director open, got {other:?}"),
        };
        let worker_cmds = engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: director.clone(),
                agent: None,
            },
            now,
        );
        assert_eq!(state.member_token_for(&director), Some("director"));
        let worker = worker_cmds
            .iter()
            .find_map(|cmd| match cmd {
                Cmd::OpenEmbedded { key, .. } => Some(key.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a Worker open, got {worker_cmds:?}"));
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: worker.clone(),
                agent: None,
            },
            now,
        );
        let Stage::Brigade { panes, .. } = &mut state.stage else {
            panic!("expected a staged brigade");
        };
        *panes = vec![worker.clone(), director.clone()];
        assert_eq!(state.tile_label(&director), "director");
        assert_eq!(state.tile_label(&worker), "worker-1");
    }

    #[test]
    fn tile_label_labels_the_goinkyo_pane_by_role_not_position() {
        // Drives the real spawn path (`Event::GoinkyoAwaitingSpawn` then
        // `Event::Spawned`) rather than poking at `goinkyo_pane` directly —
        // it is private to `banto-core`, reachable from here only through
        // the public events that populate it. This is the exact bug an
        // operator hit before this fix: a Goinkyo pane showed up titled
        // "worker 3".
        let director = SessionKey::from_id("dir");
        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(director.clone()),
            panes: vec![director],
            focused: 0,
        };
        let mut app = App::new(vec![]);
        let brigade = BrigadeConfig::default();
        let now = Instant::now();

        let cmds = engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoAwaitingSpawn {
                observation: GoinkyoObservation::AwaitingSpawn(GoinkyoSpawnCandidate {
                    brigade_id: 1,
                    cwd: PathBuf::from("/work/alpha"),
                    roster: Vec::new(),
                }),
            },
            now,
        );
        let goinkyo_key = match cmds.as_slice() {
            [Cmd::OpenEmbedded { key, .. }] => key.clone(),
            other => panic!("expected exactly one OpenEmbedded: {other:?}"),
        };
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: goinkyo_key.clone(),
                agent: None,
            },
            now,
        );

        assert_eq!(state.tile_label(&goinkyo_key), "goinkyo");
    }

    // --- window_focus_event: the main loop's FocusGained/FocusLost translation --

    #[test]
    fn a_raw_focus_gained_becomes_window_focus_changed_true() {
        assert_eq!(
            window_focus_event(&crossterm::event::Event::FocusGained),
            Some(Event::WindowFocusChanged { focused: true })
        );
    }

    #[test]
    fn a_raw_focus_lost_becomes_window_focus_changed_false() {
        assert_eq!(
            window_focus_event(&crossterm::event::Event::FocusLost),
            Some(Event::WindowFocusChanged { focused: false })
        );
    }

    #[test]
    fn every_other_raw_event_kind_is_not_a_window_focus_change() {
        // A key must fall through to `convert::from_crossterm` — if this
        // ever started matching keys too, they'd stop reaching the paste
        // accumulator entirely.
        assert_eq!(
            window_focus_event(&crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('a'),
                    crossterm::event::KeyModifiers::NONE,
                )
            )),
            None
        );
        assert_eq!(
            window_focus_event(&crossterm::event::Event::Paste("x".to_string())),
            None
        );
    }

    // --- read_or_idle_tick: drain-before-tick --------------------------

    fn stamped_key(c: char, at: Instant) -> StampedEvent {
        StampedEvent {
            event: crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            )),
            at,
        }
    }

    fn key(c: char) -> banto_core::input::KeyEvent {
        banto_core::input::KeyEvent::new(
            banto_core::input::KeyCode::Char(c),
            banto_core::input::Modifiers::NONE,
        )
    }

    #[test]
    fn a_queued_event_is_returned_with_the_reader_threads_own_stamp() {
        let (tx, rx) = mpsc::channel();
        let mut acc = PasteAccumulator::new();
        let t0 = Instant::now();
        // Mirrors mid-burst state: a first key already buffered.
        acc.accept(key('a'), t0);
        // A second key, queued with its own close stamp — independent of
        // whatever this test does before calling `read_or_idle_tick`.
        tx.send(stamped_key('b', t0 + Duration::from_millis(2)))
            .unwrap();

        let step = read_or_idle_tick(&rx, Duration::from_millis(10), &mut acc).unwrap();
        match step {
            InputStep::Received(stamped) => {
                assert_eq!(stamped.at, t0 + Duration::from_millis(2));
            }
            InputStep::Idle(_) => panic!("expected the queued event, not an idle tick"),
        }
        assert!(
            acc.is_pending(),
            "the first key must still be buffered, not flushed by a spurious tick"
        );
    }

    #[test]
    fn a_queued_event_wins_over_a_buffer_that_only_looks_stale_by_wall_clock() {
        // The hazard this function exists to close: judging staleness by
        // elapsed wall time *before* checking the channel would wrongly
        // flush here, splitting a run that the reader thread's own stamps
        // show was never actually slow.
        let (tx, rx) = mpsc::channel();
        let mut acc = PasteAccumulator::new();
        let long_ago = Instant::now() - Duration::from_millis(200);
        acc.accept(key('a'), long_ago);
        tx.send(stamped_key('b', long_ago + Duration::from_millis(2)))
            .unwrap();

        let step = read_or_idle_tick(&rx, Duration::from_millis(10), &mut acc).unwrap();
        assert!(
            matches!(step, InputStep::Received(_)),
            "a queued event must win over a wall-clock staleness check every time"
        );
        assert!(acc.is_pending(), "must not have been flushed");
    }

    #[test]
    fn a_genuinely_empty_channel_flushes_a_stale_buffer() {
        let (_tx, rx) = mpsc::channel::<StampedEvent>();
        let mut acc = PasteAccumulator::new();
        acc.accept(key('a'), Instant::now() - Duration::from_millis(200));

        let step = read_or_idle_tick(&rx, Duration::from_millis(1), &mut acc).unwrap();
        match step {
            InputStep::Idle(Some(InputEvent::Key(k))) => assert_eq!(k, key('a')),
            other => panic!("expected a flush of the stale buffer, got {other:?}"),
        }
        assert!(!acc.is_pending());
    }

    #[test]
    fn an_empty_channel_with_nothing_buffered_is_idle_with_no_flush() {
        let (_tx, rx) = mpsc::channel::<StampedEvent>();
        let mut acc = PasteAccumulator::new();

        let step = read_or_idle_tick(&rx, Duration::from_millis(1), &mut acc).unwrap();
        assert!(matches!(step, InputStep::Idle(None)));
    }

    #[test]
    fn a_disconnected_reader_thread_is_reported_as_an_error() {
        let (tx, rx) = mpsc::channel::<StampedEvent>();
        drop(tx);
        let mut acc = PasteAccumulator::new();

        assert!(read_or_idle_tick(&rx, Duration::from_millis(1), &mut acc).is_err());
    }

    // --- close_any_open_hyperlink: the per-frame OSC 8 close -------------
    //
    // Proves the bytes and the flush call are right; it cannot exercise
    // "runs once per frame after terminal.draw" — that's `event_loop`
    // itself, which takes a real `Terminal<CrosstermBackend<Stdout>>` and
    // has no test anywhere in this file driving it directly (a barrier
    // that predates this change).

    // --- the bell log ---------------------------------------------------------

    fn quiet_cause() -> banto_core::engine::BellCause {
        banto_core::engine::BellCause::QuietRecorded {
            brigade: 1,
            director: SessionKey::from_id("dir"),
        }
    }

    /// A sink that fails every write, as a terminal that went away would.
    struct BrokenSink;

    impl io::Write for BrokenSink {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn log_lines(file: &mut std::fs::File) -> Vec<String> {
        use std::io::{Read as _, Seek as _};
        file.rewind().unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn a_variant_is_named_without_formatting_what_it_carries() {
        let key = SessionKey::from_id("a");
        // A megabyte of output must not be walked to find out which event it is.
        let output = Event::PtyOutput {
            key,
            chunk: vec![7; 1 << 20],
        };
        assert_eq!(variant_name(&output), "PtyOutput");
        assert_eq!(
            variant_name(&Event::Resized {
                width: 80,
                height: 24
            }),
            "Resized"
        );
        assert_eq!(
            variant_name(&Event::Tick {
                relay: Vec::new(),
                brigades: Vec::new()
            }),
            "Tick"
        );
    }

    #[test]
    fn a_bell_is_written_once_and_logged_with_its_cause_and_the_event_behind_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(dir.path().join("bell.log"))
            .unwrap();
        let mut host = Vec::new();
        ring_host_bell_to(
            &mut host,
            &quiet_cause(),
            Some(BellEntry {
                file: &mut file,
                since_start: Duration::from_millis(5123),
                event: "QuiescenceSampled",
            }),
        );
        assert_eq!(host, b"\x07", "one BEL and nothing else");
        let lines = log_lines(&mut file);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let (wall, rest) = lines[0].split_once(' ').unwrap();
        assert!(wall.chars().all(|c| c.is_ascii_digit()), "{}", lines[0]);
        assert_eq!(
            rest,
            "emporium: bell +5123ms cause=quiet_recorded brigade=1 director=dir \
             event=QuiescenceSampled written"
        );
    }

    #[test]
    fn a_bell_that_could_not_be_written_is_logged_as_such_and_breaks_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(dir.path().join("bell.log"))
            .unwrap();
        ring_host_bell_to(
            &mut BrokenSink,
            &quiet_cause(),
            Some(BellEntry {
                file: &mut file,
                since_start: Duration::ZERO,
                event: "Tick",
            }),
        );
        let lines = log_lines(&mut file);
        assert!(
            lines[0].ends_with("event=Tick failed=BrokenPipe"),
            "{lines:?}"
        );
    }

    #[test]
    fn a_key_that_is_not_tidy_cannot_split_or_widen_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(dir.path().join("bell.log"))
            .unwrap();
        let cause = banto_core::engine::BellCause::StagedPaneBell {
            pane: SessionKey::from_id("a\nb\r\x1b[2Jc"),
            others: 0,
        };
        ring_host_bell_to(
            &mut Vec::new(),
            &cause,
            Some(BellEntry {
                file: &mut file,
                since_start: Duration::ZERO,
                event: "Tick",
            }),
        );
        let lines = log_lines(&mut file);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("pane=a_b__[2Jc"), "{lines:?}");
        assert!(!lines[0].chars().any(char::is_control), "{lines:?}");
    }

    #[test]
    fn with_no_log_the_bell_is_just_the_bell() {
        let mut host = Vec::new();
        ring_host_bell_to(&mut host, &quiet_cause(), None);
        assert_eq!(host, b"\x07");
        // And a host that fails is not an error anywhere.
        ring_host_bell_to(&mut BrokenSink, &quiet_cause(), None);
    }

    #[test]
    fn close_any_open_hyperlink_writes_the_bare_osc_8_close_and_flushes() {
        let mut sink = Vec::new();
        close_any_open_hyperlink(&mut sink).unwrap();
        assert_eq!(sink, b"\x1b]8;;\x1b\\");
    }

    // --- BANTO_INPUT_LOG: paste payloads never reach the log line --------

    #[test]
    fn describe_raw_event_reports_a_paste_length_not_its_text() {
        let secret = "line one\nline two\nsome pasted secret".to_string();
        let described = describe_raw_event(&crossterm::event::Event::Paste(secret.clone()));
        assert_eq!(described, format!("raw paste len={}", secret.len()));
        assert!(!described.contains("secret"));
    }

    #[test]
    fn describe_converted_event_reports_a_paste_length_not_its_text() {
        let secret = "another\npasted\nsecret".to_string();
        let described = describe_converted_event(&InputEvent::Paste(secret.clone()));
        assert_eq!(described, format!("converted paste len={}", secret.len()));
        assert!(!described.contains("secret"));
    }

    // --- BANTO_RECORD_EVENTS: EventRecorder ------------------------------

    #[test]
    fn event_recorder_writes_the_header_once_then_appends_events_replay_can_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.jsonl");
        let run_start = Instant::now();

        let mut recorder = EventRecorder::open(&path, run_start).unwrap();
        recorder.record(
            &Event::Resized {
                width: 80,
                height: 24,
            },
            run_start + Duration::from_millis(50),
        );
        recorder.record(
            &Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            run_start + Duration::from_millis(1200),
        );
        drop(recorder);

        // Reopening at the same (now non-empty) path must not write a
        // second header line mid-file.
        let mut recorder = EventRecorder::open(&path, run_start).unwrap();
        recorder.record(
            &Event::Resized {
                width: 10,
                height: 10,
            },
            run_start + Duration::from_millis(2000),
        );
        drop(recorder);

        let text = std::fs::read_to_string(&path).unwrap();
        let events = banto_core::replay::parse_stream(&text).expect("a well-formed stream");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].offset_ms, 50);
        assert_eq!(events[1].offset_ms, 1200);
        assert_eq!(events[2].offset_ms, 2000);
        assert_eq!(
            events[0].event,
            Event::Resized {
                width: 80,
                height: 24
            }
        );
    }

    #[test]
    fn event_recorder_never_backdates_an_offset_when_now_precedes_run_start() {
        // `saturating_duration_since` — a defensive floor, not an expected
        // case: `now` in the real event loop is always >= `run_start`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.jsonl");
        let run_start = Instant::now() + Duration::from_secs(1);

        let mut recorder = EventRecorder::open(&path, run_start).unwrap();
        recorder.record(
            &Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            run_start - Duration::from_secs(1),
        );
        drop(recorder);

        let text = std::fs::read_to_string(&path).unwrap();
        let events = banto_core::replay::parse_stream(&text).unwrap();
        assert_eq!(events[0].offset_ms, 0);
    }

    // --- paint_pane: honoring DECSET 2026 without freezing forever --------

    /// `paint_pane` into a fresh `Buffer` sized to `content` and read the
    /// one row back as a string — the tests below use a single-row screen,
    /// so this is the whole visible pane.
    fn paint_row(
        cache: &mut HashMap<SessionKey, PaneRenderCache>,
        key: &SessionKey,
        screen: &Screen,
        content: Rect,
        tick: Instant,
    ) -> String {
        let mut frame_buffer = Buffer::empty(content);
        paint_pane(&mut frame_buffer, cache, key, screen, content, tick);
        (content.x..content.x + content.width)
            .map(|x| frame_buffer[(x, content.y)].symbol().to_string())
            .collect()
    }

    #[test]
    fn a_pane_mid_synchronized_update_keeps_showing_its_last_complete_frame() {
        let mut screen = Screen::new(1, 10);
        screen.process(b"AAAAAAAAAA");
        let mut cache = HashMap::new();
        let key = SessionKey::from_id("s1");
        let content = Rect::new(0, 0, 10, 1);
        let tick = Instant::now();

        // Prime the cache with the pre-update frame.
        let pre = paint_row(&mut cache, &key, &screen, content, tick);
        assert_eq!(pre, "AAAAAAAAAA");

        // Open a synchronized update and overwrite the whole row mid-update.
        screen.process(b"\x1b[?2026h\x1b[HBBBBBBBBBB");
        let mid = paint_row(&mut cache, &key, &screen, content, tick);
        assert_eq!(
            mid, "AAAAAAAAAA",
            "must keep showing the pre-update frame while the block is open"
        );

        // Close it: the next draw catches up to what actually happened.
        screen.process(b"\x1b[?2026l");
        let post = paint_row(&mut cache, &key, &screen, content, tick);
        assert_eq!(post, "BBBBBBBBBB");
    }

    #[test]
    fn a_pane_mid_synchronized_update_keeps_showing_its_last_complete_cursor_too() {
        let mut screen = Screen::new(1, 10);
        screen.process(b"AAAAA"); // cursor at column 5 after the pre-update frame
        let mut cache = HashMap::new();
        let key = SessionKey::from_id("s1");
        let content = Rect::new(0, 0, 10, 1);
        let tick = Instant::now();

        let mut buf = Buffer::empty(content);
        let pre = paint_pane(&mut buf, &mut cache, &key, &screen, content, tick);
        assert_eq!(pre, Some((5, 0)));

        // Open a synchronized update and move the cursor mid-update.
        screen.process(b"\x1b[?2026h\rBB");
        let mut buf = Buffer::empty(content);
        let mid = paint_pane(&mut buf, &mut cache, &key, &screen, content, tick);
        assert_eq!(
            mid,
            Some((5, 0)),
            "must keep the pre-update cursor position while the block is open"
        );

        // Close it: the next draw catches up to where the cursor actually is.
        screen.process(b"\x1b[?2026l");
        let mut buf = Buffer::empty(content);
        let post = paint_pane(&mut buf, &mut cache, &key, &screen, content, tick);
        assert_eq!(post, Some((2, 0)));
    }

    #[test]
    fn a_pane_mid_synchronized_update_keeps_the_cursor_hidden_if_it_was_hidden_before() {
        let mut screen = Screen::new(1, 10);
        screen.process(b"AAAAA\x1b[?25l"); // hide the cursor before the update opens
        let mut cache = HashMap::new();
        let key = SessionKey::from_id("s1");
        let content = Rect::new(0, 0, 10, 1);
        let tick = Instant::now();

        let mut buf = Buffer::empty(content);
        let pre = paint_pane(&mut buf, &mut cache, &key, &screen, content, tick);
        assert_eq!(pre, None, "hidden before the update opened");

        // Mid-update, the child shows the cursor again — must not leak
        // through while the block is still open.
        screen.process(b"\x1b[?2026h\x1b[?25h");
        let mut buf = Buffer::empty(content);
        let mid = paint_pane(&mut buf, &mut cache, &key, &screen, content, tick);
        assert_eq!(
            mid, None,
            "must not show a cursor the pre-update frame didn't have"
        );
    }

    #[test]
    fn a_synchronized_update_held_past_the_timeout_draws_live_again() {
        let mut screen = Screen::new(1, 10);
        screen.process(b"AAAAAAAAAA");
        let mut cache = HashMap::new();
        let key = SessionKey::from_id("s1");
        let content = Rect::new(0, 0, 10, 1);
        let t0 = Instant::now();
        paint_row(&mut cache, &key, &screen, content, t0);

        // Opened, mutated, and never closed — a hung or dead child.
        screen.process(b"\x1b[?2026h\x1b[HBBBBBBBBBB");
        let still_honored = paint_row(&mut cache, &key, &screen, content, t0);
        assert_eq!(still_honored, "AAAAAAAAAA");

        let past_deadline = t0 + SYNC_UPDATE_TIMEOUT + Duration::from_millis(1);
        let recovered = paint_row(&mut cache, &key, &screen, content, past_deadline);
        assert_eq!(
            recovered, "BBBBBBBBBB",
            "a child that never closes its update must not freeze its pane forever"
        );
    }

    #[test]
    fn a_synchronized_update_within_the_deadline_stays_frozen_at_the_next_poll() {
        // Regression: the deadline must be measured from when the block
        // *opened*, not reset on every poll that finds it still open.
        let mut screen = Screen::new(1, 10);
        screen.process(b"AAAAAAAAAA");
        let mut cache = HashMap::new();
        let key = SessionKey::from_id("s1");
        let content = Rect::new(0, 0, 10, 1);
        let t0 = Instant::now();
        paint_row(&mut cache, &key, &screen, content, t0);

        screen.process(b"\x1b[?2026h\x1b[HBBBBBBBBBB");
        paint_row(&mut cache, &key, &screen, content, t0);

        let still_within = t0 + SYNC_UPDATE_TIMEOUT - Duration::from_millis(1);
        let text = paint_row(&mut cache, &key, &screen, content, still_within);
        assert_eq!(text, "AAAAAAAAAA");
    }

    #[test]
    fn a_resize_mid_hold_paints_live_instead_of_blitting_the_wrong_size() {
        // The cached buffer was captured at the old `content` rect; blitting
        // it into a differently-sized one would misplace or truncate cells,
        // so a size mismatch must fall back to a live repaint rather than
        // trust a cache that no longer describes this pane's shape.
        let mut screen = Screen::new(1, 10);
        screen.process(b"AAAAAAAAAA");
        let mut cache = HashMap::new();
        let key = SessionKey::from_id("s1");
        let small = Rect::new(0, 0, 10, 1);
        let t0 = Instant::now();
        paint_row(&mut cache, &key, &screen, small, t0);

        screen.process(b"\x1b[?2026h");
        screen.resize(1, 12);
        screen.process(b"\x1b[HBBBBBBBBBBBB");
        let wide = Rect::new(0, 0, 12, 1);
        let mut frame_buffer = Buffer::empty(wide);
        paint_pane(&mut frame_buffer, &mut cache, &key, &screen, wide, t0);
        let row: String = (0..12)
            .map(|x| frame_buffer[(x, 0)].symbol().to_string())
            .collect();
        assert_eq!(row, "BBBBBBBBBBBB");
    }

    #[test]
    fn a_pane_already_mid_update_on_its_very_first_paint_shows_blank_not_the_partial_frame() {
        // There is no earlier known-good frame for a pane that has never
        // been painted before, so honoring the hold here means blank, not
        // whatever happens to be in the grid mid-draw.
        let mut screen = Screen::new(1, 10);
        screen.process(b"\x1b[?2026hAAAAAAAAAA"); // mid-update from the very first byte
        let mut cache = HashMap::new();
        let key = SessionKey::from_id("s1");
        let content = Rect::new(0, 0, 10, 1);

        let first = paint_row(&mut cache, &key, &screen, content, Instant::now());
        assert_eq!(first, "          ", "must not leak the in-progress frame");
    }

    // --- what actually reaches the PTY host ------------------------------
    //
    // `Cmd::OpenEmbedded` has seven producers in `engine.rs` and they all
    // funnel through one `PtyHandle::open` call. Until `Deps` carried a
    // `PtyHost`, that call named `&PortablePtyHost` itself and no test could
    // see what it passed — which is how `cf519a7` came to establish an
    // argument of that very call by re-measuring a live `claude` instead.

    /// A `Deps` whose PTY host is a mock, plus the pieces it borrows. Kept
    /// as one struct because `Deps` is all references and a helper returning
    /// it alone would borrow locals that die at the end of the helper.
    struct SpawnFixture {
        store: RefCell<Store>,
        meta_cache: MetaCache,
        pty_host: MockPtyHost,
        input_reader: MockInputReader,
        thresholds: AgeThresholds,
        brigade: BrigadeConfig,
        claude_home: ClaudeHome,
        agent_binaries: AgentBinaries,
        enabled_agents: BTreeSet<AgentKind>,
    }

    impl SpawnFixture {
        fn new(claude_home: PathBuf) -> Self {
            Self {
                store: RefCell::new(Store::open_in_memory().unwrap()),
                meta_cache: MetaCache::new(),
                pty_host: MockPtyHost::default(),
                input_reader: MockInputReader::default(),
                thresholds: AgeThresholds::default(),
                brigade: BrigadeConfig::default(),
                claude_home: ClaudeHome::new(claude_home),
                agent_binaries: AgentBinaries::default(),
                enabled_agents: all_agents(),
            }
        }

        fn deps(&self) -> Deps<'_> {
            Deps {
                claude_home: &self.claude_home,
                codex_home: None,
                thresholds: &self.thresholds,
                store: &self.store,
                meta_cache: &self.meta_cache,
                pty_host: &self.pty_host,
                input_reader: &self.input_reader,
                brigade: &self.brigade,
                agent_binaries: &self.agent_binaries,
                enabled_agents: &self.enabled_agents,
                codex_catalog: &NoCatalog,
                child: None,
                exe: crate::exe::test_support::current(),
            }
        }

        fn launches(&self) -> Vec<banto_io::pty::mock::MockLaunch> {
            self.pty_host.launches.lock().unwrap().clone()
        }

        fn env_removes(&self) -> Vec<Vec<String>> {
            self.pty_host.env_removes.lock().unwrap().clone()
        }
    }

    fn open_embedded_cmd(agent: AgentKind, id: &str, cwd: &Path) -> Cmd {
        Cmd::OpenEmbedded {
            key: SessionKey::from_id("pane"),
            target: SessionToOpen {
                id: id.to_string(),
                agent,
                title: "a session".to_string(),
                cwd: cwd.to_path_buf(),
            },
            brigade: None,
            model: None,
            effort: None,
            permission_mode: None,
            disallowed_tools: None,
            autocompact: None,
        }
    }

    #[test]
    fn lost_output_retains_handle_until_waiter_confirms_exit() {
        struct FailingKiller;
        impl banto_io::pty::Killer for FailingKiller {
            fn kill(&mut self) -> anyhow::Result<()> {
                anyhow::bail!("synthetic kill failure")
            }
        }
        struct FailingHost(MockPtyHost);
        impl PtyHost for FailingHost {
            fn open(
                &self,
                argv: &[String],
                cwd: Option<&Path>,
                env: &[(String, String)],
                removed: &[&str],
                rows: u16,
                cols: u16,
            ) -> anyhow::Result<banto_io::pty::PtyIo> {
                let mut io = self.0.open(argv, cwd, env, removed, rows, cols)?;
                io.killer = Box::new(FailingKiller);
                Ok(io)
            }
        }

        let host = FailingHost(MockPtyHost {
            unix_style_exit: true,
            ..Default::default()
        });
        let mut row = test_row("existing", AgentKind::Codex);
        row.cwd = Some(PathBuf::from("/synthetic"));
        let mut app = App::new(vec![row]);
        app.open_new_session_modal();
        app.modal_toggle_agent();
        let cwd = app.modal_new_session_target().unwrap();
        app.modal_begin_new_session_check();
        let mut state = EmporiumState::new(PrefixKey::default());
        let brigade = BrigadeConfig::default();
        let cmds = engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::NewSessionCwdChecked { cwd, is_dir: true },
            Instant::now(),
        );
        let key = cmds
            .iter()
            .find_map(|cmd| match cmd {
                Cmd::OpenEmbedded { key, .. } => Some(key.clone()),
                _ => None,
            })
            .unwrap();
        let mut handle =
            PtyHandle::open(&host, &["synthetic".into()], None, &[], &[], 24, 80).unwrap();
        handle.skip_output_loss_grace();
        let mut handles = HashMap::from([(key.clone(), handle)]);
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: key.clone(),
                agent: None,
            },
            Instant::now(),
        );
        let mut events = VecDeque::new();
        pump_pty_handles(&mut handles, &mut events);
        assert!(matches!(events.front(), Some(Event::PtyOutputLost { key: k }) if k == &key));
        let cmds = engine::update(
            &mut state,
            &mut app,
            &brigade,
            events.pop_front().unwrap(),
            Instant::now(),
        );
        // A pane whose output is gone is hung up like any other, and the force
        // still follows when it does not leave: the screen is not what the
        // deadline hangs on.
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, Cmd::HangUpPty { key: k } if k == &key))
        );
        assert!(!cmds.iter().any(|cmd| matches!(cmd, Cmd::KillPty { .. })));
        let forced = engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: Vec::new(),
                brigades: Vec::new(),
            },
            Instant::now() + engine::PANE_END_GRACE + Duration::from_secs(1),
        );
        assert!(
            forced
                .iter()
                .any(|cmd| matches!(cmd, Cmd::KillPty { key: k } if k == &key))
        );
        assert!(handles.get_mut(&key).unwrap().kill().is_err());
        assert!(!state.screens.contains_key(&key));
        assert!(reap_pty_handles(&mut handles, &state).is_empty());
        let activate = |state: &mut EmporiumState, app: &mut App| {
            let requested = engine::update(
                state,
                app,
                &brigade,
                key_event(
                    banto_core::input::KeyCode::Enter,
                    banto_core::input::Modifiers::NONE,
                ),
                Instant::now(),
            );
            assert!(
                requested
                    .iter()
                    .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::ResolveMembership { .. })))
            );
            engine::update(
                state,
                app,
                &brigade,
                Event::MembershipResolved {
                    session_id: "existing".into(),
                    membership: None,
                    members: None,
                    layout: None,
                    names: Vec::new(),
                },
                Instant::now(),
            )
        };
        assert!(
            !activate(&mut state, &mut app)
                .iter()
                .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. }))
        );
        pump_pty_handles(&mut handles, &mut events);
        assert!(events.is_empty());
        assert!(reap_pty_handles(&mut handles, &state).is_empty());
        host.0.fire_exit();
        pump_pty_handles(&mut handles, &mut events);
        assert!(matches!(events.front(), Some(Event::PtyExited { key: k, .. }) if k == &key));
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            events.pop_front().unwrap(),
            Instant::now(),
        );
        assert_eq!(reap_pty_handles(&mut handles, &state).len(), 1);
        assert!(handles.is_empty());
        assert!(
            activate(&mut state, &mut app).iter().any(
                |cmd| matches!(cmd, Cmd::OpenEmbedded { target, .. } if target.id == "existing")
            )
        );
    }

    #[test]
    fn opening_a_pane_strips_the_marker_that_hides_a_session_from_banto() {
        // The argument `cf519a7` had to prove by re-measurement. A call site
        // that drops it produces a session banto's activity indicator can
        // never see, and nothing else about the launch looks wrong.
        let dir = tempfile::tempdir().unwrap();
        let fixture = SpawnFixture::new(PathBuf::from("/nonexistent"));
        let mut handles = HashMap::new();
        let mut discovery = Vec::new();

        let events = execute_cmd(
            open_embedded_cmd(AgentKind::ClaudeCode, "", dir.path()),
            &fixture.deps(),
            &mut handles,
            &mut discovery,
        );

        assert_eq!(
            events,
            vec![Event::Spawned {
                key: SessionKey::from_id("pane"),
                agent: Some(AgentKind::ClaudeCode),
            }]
        );
        assert_eq!(
            fixture.env_removes(),
            vec![
                STRIPPED_CHILD_ENV_VARS
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>()
            ]
        );
    }

    #[test]
    fn a_claude_pane_is_launched_with_the_native_cursor_and_a_codex_pane_is_not() {
        // The variable is what keeps the input line readable (see `pane_env`), and
        // no gate notices a launch that loses it.
        let dir = tempfile::tempdir().unwrap();
        for (agent, id) in [
            (AgentKind::ClaudeCode, ""),
            (AgentKind::ClaudeCode, "resumed"),
            (AgentKind::Codex, ""),
        ] {
            let fixture = SpawnFixture::new(PathBuf::from("/nonexistent"));
            let mut handles = HashMap::new();
            let mut discovery = Vec::new();

            execute_cmd(
                open_embedded_cmd(agent, id, dir.path()),
                &fixture.deps(),
                &mut handles,
                &mut discovery,
            );

            let launches = fixture.launches();
            assert_eq!(launches.len(), 1, "{agent:?} {id:?}");
            let native_cursor: Vec<_> = launches[0]
                .env
                .iter()
                .filter(|(name, _)| name == "CLAUDE_CODE_NATIVE_CURSOR")
                .collect();
            if agent == AgentKind::ClaudeCode {
                assert_eq!(
                    native_cursor,
                    [&("CLAUDE_CODE_NATIVE_CURSOR".to_string(), "1".to_string())],
                    "{id:?}"
                );
            } else {
                assert!(native_cursor.is_empty(), "{:?}", launches[0].env);
            }
        }
    }

    #[test]
    fn a_brigade_members_claude_pane_keeps_its_identity_beside_the_native_cursor() {
        let brigade = (7, "worker-1".to_string(), BrigadeRole::Worker);
        assert_eq!(
            pane_env(AgentKind::ClaudeCode, Some(&brigade), None),
            [
                brigade_env(Some(&brigade), None),
                vec![("CLAUDE_CODE_NATIVE_CURSOR".to_string(), "1".to_string())]
            ]
            .concat()
        );
        assert_eq!(
            pane_env(AgentKind::Codex, Some(&brigade), None),
            brigade_env(Some(&brigade), None)
        );
        assert_eq!(
            pane_env(AgentKind::ClaudeCode, None, None),
            [("CLAUDE_CODE_NATIVE_CURSOR".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn opening_a_pane_starts_the_launch_that_was_built_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = SpawnFixture::new(PathBuf::from("/nonexistent"));
        let mut handles = HashMap::new();
        let mut discovery = Vec::new();

        execute_cmd(
            open_embedded_cmd(AgentKind::ClaudeCode, "", dir.path()),
            &fixture.deps(),
            &mut handles,
            &mut discovery,
        );

        let launches = fixture.launches();
        assert_eq!(launches.len(), 1, "one pane, one child");
        assert_eq!(
            launches[0].cwd.as_deref(),
            Some(dir.path()),
            "a pane starts where its session lives"
        );
        assert!(
            launches[0]
                .argv
                .first()
                .is_some_and(|exe| exe.contains("claude")),
            "the agent binary, not something else: {:?}",
            launches[0].argv
        );
        assert!(
            !launches[0].argv.iter().any(|arg| arg == "--resume"),
            "a fresh pane resumes nothing: {:?}",
            launches[0].argv
        );
    }

    #[test]
    fn a_pane_that_never_reached_the_host_reports_a_failure_instead() {
        // The other half of the seam: `build_open_launch` refusing (here,
        // because the session id is already live elsewhere — AGENTS.md
        // invariant 4) must not reach the host at all.
        let dir = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let sessions = claude_home.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let pid = std::process::id();
        std::fs::write(
            sessions.join(format!("{pid}.json")),
            format!("{{\"sessionId\":\"live-1\",\"pid\":{pid},\"status\":\"idle\"}}"),
        )
        .unwrap();
        let fixture = SpawnFixture::new(claude_home.path().to_path_buf());
        let mut handles = HashMap::new();
        let mut discovery = Vec::new();

        let events = execute_cmd(
            open_embedded_cmd(AgentKind::ClaudeCode, "live-1", dir.path()),
            &fixture.deps(),
            &mut handles,
            &mut discovery,
        );

        assert!(
            matches!(&events[..], [Event::SpawnFailed { .. }]),
            "a session already open elsewhere must not be resumed twice: {events:?}"
        );
        assert!(
            fixture.launches().is_empty(),
            "and nothing may reach the host: {:?}",
            fixture.launches()
        );
        assert!(handles.is_empty());
    }

    /// `open_embedded_cmd` carrying `autocompact`.
    fn open_with_autocompact(
        agent: AgentKind,
        id: &str,
        cwd: &Path,
        autocompact: Option<&str>,
    ) -> Cmd {
        let Cmd::OpenEmbedded {
            key,
            target,
            brigade,
            model,
            effort,
            permission_mode,
            disallowed_tools,
            ..
        } = open_embedded_cmd(agent, id, cwd)
        else {
            unreachable!()
        };
        Cmd::OpenEmbedded {
            key,
            target,
            brigade,
            model,
            effort,
            permission_mode,
            disallowed_tools,
            autocompact: autocompact.map(str::to_string),
        }
    }

    /// The argv the one pane `cmd` started was launched with.
    fn launched_argv(cmd: Cmd) -> Vec<String> {
        let fixture = SpawnFixture::new(PathBuf::from("/nonexistent"));
        let mut handles = HashMap::new();
        let mut discovery = Vec::new();
        execute_cmd(cmd, &fixture.deps(), &mut handles, &mut discovery);
        let launches = fixture.launches();
        assert_eq!(launches.len(), 1, "one pane, one child");
        launches[0].argv.clone()
    }

    #[test]
    fn a_claude_pane_is_launched_with_autocompact_whether_it_is_new_or_resumed() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["", "claude-session"] {
            let argv = launched_argv(open_with_autocompact(
                AgentKind::ClaudeCode,
                id,
                dir.path(),
                Some("250000"),
            ));
            let at = argv
                .iter()
                .position(|arg| arg == "--autocompact")
                .unwrap_or_else(|| panic!("no --autocompact for id {id:?}: {argv:?}"));
            assert_eq!(argv[at + 1], "250000", "{argv:?}");
        }
    }

    #[test]
    fn a_claude_pane_with_no_autocompact_is_launched_without_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["", "claude-session"] {
            let argv = launched_argv(open_with_autocompact(
                AgentKind::ClaudeCode,
                id,
                dir.path(),
                None,
            ));
            assert!(!argv.iter().any(|arg| arg == "--autocompact"), "{argv:?}");
        }
    }

    #[test]
    fn a_codex_pane_is_launched_with_its_token_limit_override_new_or_resumed() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["", "codex-session"] {
            let argv = launched_argv(open_with_autocompact(
                AgentKind::Codex,
                id,
                dir.path(),
                Some("250000"),
            ));
            let at = argv
                .iter()
                .position(|arg| arg == "model_auto_compact_token_limit=250000")
                .unwrap_or_else(|| panic!("no override for id {id:?}: {argv:?}"));
            assert_eq!(argv[at - 1], "-c", "{argv:?}");
            assert!(
                !argv.iter().any(|arg| arg == "--autocompact"),
                "Claude's flag is not Codex's: {argv:?}"
            );
        }
    }

    #[test]
    fn a_codex_pane_with_no_autocompact_is_launched_without_the_override() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["", "codex-session"] {
            let argv = launched_argv(open_with_autocompact(
                AgentKind::Codex,
                id,
                dir.path(),
                None,
            ));
            assert!(
                !argv.iter().any(|arg| arg.contains("auto_compact")),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn build_open_launch_gives_a_claude_target_its_autocompact_on_creation_and_on_resume() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        for id in ["", "sess-1"] {
            let launch = build_open_launch(
                &open_target(id),
                None,
                None,
                RoleFlags {
                    autocompact: Some("auto"),
                    ..RoleFlags::default()
                },
                None,
                &test_ctx(&probe, &[]),
            )
            .unwrap();
            assert!(
                matches!(
                    &launch,
                    launch::AgentLaunch::Claude { autocompact: Some(value), .. } if value == "auto"
                ),
                "{launch:?}"
            );
        }
    }

    /// `cmd` with a model and an effort, for `agent`, resuming `id` (empty for a
    /// new session).
    fn open_with_model_and_effort(agent: AgentKind, id: &str, cwd: &Path) -> Cmd {
        let Cmd::OpenEmbedded {
            key,
            target,
            brigade,
            permission_mode,
            disallowed_tools,
            autocompact,
            ..
        } = open_embedded_cmd(agent, id, cwd)
        else {
            unreachable!()
        };
        Cmd::OpenEmbedded {
            key,
            target,
            brigade,
            model: Some("the-model".to_string()),
            effort: Some("the-effort".to_string()),
            permission_mode,
            disallowed_tools,
            autocompact,
        }
    }

    #[test]
    fn a_new_session_is_launched_with_its_model_and_effort_and_a_resumed_one_with_neither_for_both_products()
     {
        let dir = tempfile::tempdir().unwrap();
        for (agent, id) in [
            (AgentKind::ClaudeCode, ""),
            (AgentKind::ClaudeCode, "claude-session"),
            (AgentKind::Codex, ""),
            (AgentKind::Codex, "codex-session"),
        ] {
            let fixture = SpawnFixture::new(PathBuf::from("/nonexistent"));
            let events = execute_cmd(
                open_with_model_and_effort(agent, id, dir.path()),
                &fixture.deps(),
                &mut HashMap::new(),
                &mut Vec::new(),
            );
            assert!(
                matches!(&events[..], [Event::Spawned { agent: Some(actual), .. }] if *actual == agent),
                "{events:?}"
            );
            let argv = fixture.launches()[0].argv.clone();
            let has = |needle: &str| argv.iter().any(|arg| arg.contains(needle));
            let created = id.is_empty();
            match agent {
                AgentKind::ClaudeCode => {
                    assert_eq!(has("--model"), created, "{agent:?} {id:?}: {argv:?}");
                    assert_eq!(has("--effort"), created, "{agent:?} {id:?}: {argv:?}");
                    assert_eq!(has("--resume"), !created, "{agent:?} {id:?}: {argv:?}");
                }
                AgentKind::Codex => {
                    assert_eq!(has("the-model"), created, "{agent:?} {id:?}: {argv:?}");
                    assert_eq!(
                        has("model_reasoning_effort=\"the-effort\""),
                        created,
                        "{agent:?} {id:?}: {argv:?}"
                    );
                    assert_eq!(argv.contains(&"-m".to_string()), created, "{argv:?}");
                    assert_eq!(has("resume"), !created, "{agent:?} {id:?}: {argv:?}");
                }
            }
        }
    }

    // --- a resumed member's briefing against what Claude recorded ----------
    //
    // Claude reuses the system prompt it recorded on a conversation's first
    // request, so a resumed member is passed `--system-prompt-snapshot off`
    // exactly when its transcript does not already hold the briefing (the rule
    // and its grounds: `banto_io::claude_briefing`). Everything here is
    // synthetic: transcripts are made by hand in a temporary Claude home.

    const SNAPSHOT_OFF: [&str; 2] = ["--system-prompt-snapshot", "off"];

    /// A brigade in a store, a Claude home and a data directory that are all
    /// temporary, so a launch writes nothing outside them.
    struct BriefingRig {
        fixture: SpawnFixture,
        _claude_home: tempfile::TempDir,
        data_dir: tempfile::TempDir,
        brigade_id: BrigadeId,
    }

    impl BriefingRig {
        fn new() -> Self {
            let claude_home = tempfile::tempdir().unwrap();
            let fixture = SpawnFixture::new(claude_home.path().to_path_buf());
            let brigade_id = {
                let mut store = fixture.store.borrow_mut();
                let id = store.create_brigade("cell").unwrap();
                for (token, role) in [
                    ("director", BrigadeRole::Director),
                    ("worker-1", BrigadeRole::Worker),
                    ("goinkyo", BrigadeRole::Goinkyo),
                ] {
                    store.add_brigade_member(id, token, role, None).unwrap();
                }
                id
            };
            Self {
                fixture,
                _claude_home: claude_home,
                data_dir: tempfile::tempdir().unwrap(),
                brigade_id,
            }
        }

        fn resolved(&self) -> crate::startup::Resolved {
            crate::startup::Resolved {
                config_file: crate::startup::ConfigFile::None,
                db_path: None,
                claude_home: None,
                codex_home: None,
                data_dir: Some(self.data_dir.path().to_path_buf()),
            }
        }

        /// Records `lines` as session `id`'s transcript.
        fn transcript(&self, id: &str, lines: &[String]) {
            self.transcript_in("-work-alpha", id, lines);
        }

        /// The same, in the project folder `project`.
        fn transcript_in(&self, project: &str, id: &str, lines: &[String]) {
            let dir = self.fixture.claude_home.projects_dir().join(project);
            std::fs::create_dir_all(&dir).unwrap();
            let mut text = lines.join("\n");
            text.push('\n');
            std::fs::write(dir.join(format!("{id}.jsonl")), text).unwrap();
        }

        /// The briefing the member would be launched with, now.
        fn briefing(&self, token: &str, role: BrigadeRole) -> String {
            let resolved = self.resolved();
            let deps = Deps {
                child: Some(&resolved),
                ..self.fixture.deps()
            };
            member_briefing(&deps, self.brigade_id, token, role).unwrap()
        }

        /// The argv of the one pane opening `id` (empty: a creation) as
        /// `token` launched with.
        fn launch(
            &self,
            agent: AgentKind,
            id: &str,
            token: &str,
            role: BrigadeRole,
        ) -> Vec<String> {
            let resolved = self.resolved();
            let deps = Deps {
                child: Some(&resolved),
                ..self.fixture.deps()
            };
            let Cmd::OpenEmbedded {
                key,
                target,
                model,
                effort,
                permission_mode,
                disallowed_tools,
                autocompact,
                ..
            } = open_embedded_cmd(agent, id, self.data_dir.path())
            else {
                unreachable!()
            };
            let events = execute_cmd(
                Cmd::OpenEmbedded {
                    key,
                    target,
                    brigade: Some((self.brigade_id, token.to_string(), role)),
                    model,
                    effort,
                    permission_mode,
                    disallowed_tools,
                    autocompact,
                },
                &deps,
                &mut HashMap::new(),
                &mut Vec::new(),
            );
            assert!(matches!(&events[..], [Event::Spawned { .. }]), "{events:?}");
            self.fixture.launches().last().unwrap().argv.clone()
        }
    }

    fn snapshot_line(last_section: &str) -> String {
        serde_json::json!({
            "type": "attachment",
            "attachment": {
                "type": "prompt_snapshot",
                "systemPrompt": ["Claude's own section.", last_section],
            }
        })
        .to_string()
    }

    fn boundary_line() -> String {
        serde_json::json!({"type": "system", "subtype": "compact_boundary"}).to_string()
    }

    fn has_snapshot_off(argv: &[String]) -> bool {
        argv.windows(2).any(|pair| pair == SNAPSHOT_OFF)
    }

    const MEMBERS: [(&str, BrigadeRole); 3] = [
        ("director", BrigadeRole::Director),
        ("worker-1", BrigadeRole::Worker),
        ("goinkyo", BrigadeRole::Goinkyo),
    ];

    impl BriefingRig {
        /// Opens `token` as a member of this brigade with `exe` as the banto
        /// executable the UI resolved at start.
        fn open_member_with(
            &self,
            exe: &crate::exe::BantoExe,
            agent: AgentKind,
            token: &str,
            role: BrigadeRole,
        ) -> Vec<Event> {
            let resolved = self.resolved();
            let deps = Deps {
                child: Some(&resolved),
                exe,
                ..self.fixture.deps()
            };
            let Cmd::OpenEmbedded {
                key,
                target,
                model,
                effort,
                permission_mode,
                disallowed_tools,
                autocompact,
                ..
            } = open_embedded_cmd(agent, "", self.data_dir.path())
            else {
                unreachable!()
            };
            execute_cmd(
                Cmd::OpenEmbedded {
                    key,
                    target,
                    brigade: Some((self.brigade_id, token.to_string(), role)),
                    model,
                    effort,
                    permission_mode,
                    disallowed_tools,
                    autocompact,
                },
                &deps,
                &mut HashMap::new(),
                &mut Vec::new(),
            )
        }
    }

    /// A stand-in for the installed banto: a file, and the executable resolved
    /// from it as the UI does at start.
    fn installed_exe() -> (tempfile::TempDir, PathBuf, crate::exe::BantoExe) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("banto");
        std::fs::write(&path, b"v1").unwrap();
        let exe = crate::exe::BantoExe::at(path.clone());
        (dir, path, exe)
    }

    /// An upgrade in place: the new build renamed over the installed path.
    fn replace_on_disk(path: &Path) {
        let next = path.with_file_name("banto.new");
        std::fs::write(&next, b"v2").unwrap();
        std::fs::rename(&next, path).unwrap();
    }

    #[test]
    fn a_member_open_hands_the_agent_the_path_resolved_at_start() {
        let (_dir, installed, exe) = installed_exe();
        let rig = BriefingRig::new();

        let events =
            rig.open_member_with(&exe, AgentKind::ClaudeCode, "worker-1", BrigadeRole::Worker);
        assert!(matches!(&events[..], [Event::Spawned { .. }]), "{events:?}");
        let argv = rig.fixture.launches().last().unwrap().argv.clone();
        let config_path = &argv[argv.iter().position(|a| a == "--mcp-config").unwrap() + 1];
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
        assert_eq!(
            config["mcpServers"]["banto"]["command"],
            installed.to_string_lossy().as_ref()
        );

        let events = rig.open_member_with(&exe, AgentKind::Codex, "worker-1", BrigadeRole::Worker);
        assert!(matches!(&events[..], [Event::Spawned { .. }]), "{events:?}");
        let argv = rig.fixture.launches().last().unwrap().argv.clone();
        let forward = launch::forward_slash_path(&installed);
        assert!(
            argv.iter().any(|arg| arg.contains(&forward)),
            "the Codex overrides carry {forward}: {argv:?}"
        );
    }

    #[test]
    fn a_member_open_is_refused_once_the_executable_was_replaced_and_starts_nothing() {
        for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
            let (_dir, installed, exe) = installed_exe();
            let rig = BriefingRig::new();
            replace_on_disk(&installed);

            let events = rig.open_member_with(&exe, agent, "worker-1", BrigadeRole::Worker);

            assert!(
                matches!(
                    &events[..],
                    [Event::SpawnFailed { error, .. }]
                        if error == "banto was replaced on disk; restart banto"
                ),
                "{agent:?}: {events:?}"
            );
            assert!(
                rig.fixture.launches().is_empty(),
                "{agent:?}: nothing spawned"
            );
            assert!(
                !rig.data_dir.path().join("mcp").exists(),
                "{agent:?}: no member config written"
            );
        }
    }

    #[test]
    fn a_member_open_is_refused_when_the_executable_is_gone() {
        let (_dir, installed, exe) = installed_exe();
        let rig = BriefingRig::new();
        std::fs::remove_file(&installed).unwrap();
        let events = rig.open_member_with(
            &exe,
            AgentKind::ClaudeCode,
            "director",
            BrigadeRole::Director,
        );
        assert!(
            matches!(&events[..], [Event::SpawnFailed { error, .. }] if error.contains("replaced on disk")),
            "{events:?}"
        );
    }

    #[test]
    fn the_trust_pane_is_refused_once_the_executable_was_replaced() {
        let (_dir, installed, exe) = installed_exe();
        let rig = BriefingRig::new();
        replace_on_disk(&installed);
        let deps = Deps {
            exe: &exe,
            ..rig.fixture.deps()
        };
        let events = execute_cmd(
            Cmd::OpenCodexTrustPane {
                key: SessionKey::from_id("trust"),
            },
            &deps,
            &mut HashMap::new(),
            &mut Vec::new(),
        );
        assert!(
            matches!(&events[..], [Event::SpawnFailed { error, .. }] if error.contains("replaced on disk")),
            "{events:?}"
        );
        assert!(rig.fixture.launches().is_empty());
    }

    /// Whether the hook can carry the path is judged on the path resolved at
    /// start, however the file under it has changed since.
    #[test]
    fn the_hook_launchable_answer_comes_from_the_resolved_path() {
        let dir = tempfile::tempdir().unwrap();
        let spaced = dir.path().join("a folder").join("banto");
        std::fs::create_dir_all(spaced.parent().unwrap()).unwrap();
        std::fs::write(&spaced, b"v1").unwrap();
        assert!(!hook_launchable(&crate::exe::BantoExe::at(spaced.clone())));
        replace_on_disk(&spaced);
        assert!(!hook_launchable(&crate::exe::BantoExe::at(spaced)));
        assert!(hook_launchable(&crate::exe::BantoExe::Unknown(
            "x".to_string()
        )));
    }

    /// A pane that is not a brigade member starts no banto child, so an upgrade
    /// does not stop it opening.
    #[test]
    fn a_session_that_is_not_a_member_still_opens_after_a_replacement() {
        let (_dir, installed, exe) = installed_exe();
        let rig = BriefingRig::new();
        replace_on_disk(&installed);
        let deps = Deps {
            exe: &exe,
            ..rig.fixture.deps()
        };
        let events = execute_cmd(
            open_embedded_cmd(AgentKind::ClaudeCode, "", rig.data_dir.path()),
            &deps,
            &mut HashMap::new(),
            &mut Vec::new(),
        );
        assert!(matches!(&events[..], [Event::Spawned { .. }]), "{events:?}");
    }

    #[test]
    fn a_resumed_member_whose_transcript_holds_its_briefing_gets_no_flag() {
        for (token, role) in MEMBERS {
            let rig = BriefingRig::new();
            let briefing = rig.briefing(token, role);
            rig.transcript("sess-1", &[snapshot_line(&briefing)]);
            let argv = rig.launch(AgentKind::ClaudeCode, "sess-1", token, role);
            assert!(!has_snapshot_off(&argv), "{token}: {argv:?}");
            // The text is still passed: the flag is about the recorded copy.
            assert!(
                argv.windows(2)
                    .any(|pair| pair[0] == "--append-system-prompt" && pair[1] == briefing),
                "{token}: {argv:?}"
            );
        }
    }

    #[test]
    fn a_roster_that_changed_since_the_recording_is_not_a_different_briefing() {
        let rig = BriefingRig::new();
        let recorded = crate::briefing::compose(
            &rig.fixture.brigade,
            AgentKind::ClaudeCode,
            BrigadeRole::Director,
            rig.brigade_id,
            "director",
            &[
                "worker-1".to_string(),
                "worker-7".to_string(),
                "worker-9".to_string(),
            ],
            None,
        )
        .unwrap();
        assert_ne!(recorded, rig.briefing("director", BrigadeRole::Director));
        rig.transcript("sess-1", &[snapshot_line(&recorded)]);
        let argv = rig.launch(
            AgentKind::ClaudeCode,
            "sess-1",
            "director",
            BrigadeRole::Director,
        );
        assert!(!has_snapshot_off(&argv), "{argv:?}");
    }

    #[test]
    fn a_resumed_member_whose_recorded_briefing_differs_is_launched_with_the_flag() {
        for (token, role) in MEMBERS {
            let rig = BriefingRig::new();
            rig.transcript("sess-1", &[snapshot_line("an older briefing")]);
            let argv = rig.launch(AgentKind::ClaudeCode, "sess-1", token, role);
            let at = argv
                .iter()
                .position(|arg| arg == "--system-prompt-snapshot")
                .unwrap_or_else(|| panic!("{token}: {argv:?}"));
            assert_eq!(argv[at + 1], "off", "{token}");
            // Right after the briefing it is about, before the brigade channel.
            assert_eq!(argv[at - 2], "--append-system-prompt", "{token}: {argv:?}");
            assert_eq!(argv[at + 2], "--mcp-config", "{token}: {argv:?}");
        }
    }

    #[test]
    fn a_resumed_member_with_no_readable_record_is_launched_with_the_flag() {
        // No transcript at all, one with no snapshot, and one that is not JSON.
        let rig = BriefingRig::new();
        let argv = rig.launch(
            AgentKind::ClaudeCode,
            "absent",
            "director",
            BrigadeRole::Director,
        );
        assert!(has_snapshot_off(&argv), "{argv:?}");
        for (id, lines) in [
            (
                "no-snapshot",
                vec![serde_json::json!({"type": "user"}).to_string()],
            ),
            ("garbled", vec!["this is not json".to_string()]),
        ] {
            rig.transcript(id, &lines);
            let argv = rig.launch(AgentKind::ClaudeCode, id, "director", BrigadeRole::Director);
            assert!(has_snapshot_off(&argv), "{id}: {argv:?}");
        }
    }

    #[test]
    fn a_compaction_newer_than_the_recording_needs_no_flag() {
        let rig = BriefingRig::new();
        rig.transcript(
            "sess-1",
            &[snapshot_line("an older briefing"), boundary_line()],
        );
        let argv = rig.launch(
            AgentKind::ClaudeCode,
            "sess-1",
            "director",
            BrigadeRole::Director,
        );
        assert!(!has_snapshot_off(&argv), "{argv:?}");
        // A recording made after the compaction is compared again.
        rig.transcript(
            "sess-2",
            &[
                boundary_line(),
                snapshot_line("recorded after the compaction"),
            ],
        );
        let argv = rig.launch(
            AgentKind::ClaudeCode,
            "sess-2",
            "director",
            BrigadeRole::Director,
        );
        assert!(has_snapshot_off(&argv), "{argv:?}");
    }

    #[test]
    fn a_session_held_by_two_projects_gets_the_flag_if_either_copy_needs_it() {
        // Which copy Claude resumes from is unmeasured, so neither is trusted:
        // a newest-wins reading would skip the flag in the first case below.
        for (older, newer) in [
            ("an older briefing", "matching"),
            ("matching", "an older briefing"),
        ] {
            let rig = BriefingRig::new();
            let briefing = rig.briefing("director", BrigadeRole::Director);
            let pick = |which: &str| {
                if which == "matching" {
                    briefing.clone()
                } else {
                    which.to_string()
                }
            };
            rig.transcript_in("-work-alpha", "sess-1", &[snapshot_line(&pick(older))]);
            std::thread::sleep(std::time::Duration::from_millis(20));
            rig.transcript_in("-work-beta", "sess-1", &[snapshot_line(&pick(newer))]);
            let argv = rig.launch(
                AgentKind::ClaudeCode,
                "sess-1",
                "director",
                BrigadeRole::Director,
            );
            assert!(
                has_snapshot_off(&argv),
                "{older:?} then {newer:?}: {argv:?}"
            );
        }
        // Both copies holding the briefing is the one case that needs nothing.
        let rig = BriefingRig::new();
        let briefing = rig.briefing("director", BrigadeRole::Director);
        rig.transcript_in("-work-alpha", "sess-1", &[snapshot_line(&briefing)]);
        rig.transcript_in("-work-beta", "sess-1", &[snapshot_line(&briefing)]);
        let argv = rig.launch(
            AgentKind::ClaudeCode,
            "sess-1",
            "director",
            BrigadeRole::Director,
        );
        assert!(!has_snapshot_off(&argv), "{argv:?}");
    }

    #[test]
    fn a_creation_is_never_launched_with_the_flag() {
        for (token, role) in MEMBERS {
            let rig = BriefingRig::new();
            let argv = rig.launch(AgentKind::ClaudeCode, "", token, role);
            assert!(!has_snapshot_off(&argv), "{token}: {argv:?}");
            assert!(
                argv.contains(&"--append-system-prompt".to_string()),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn a_launch_with_no_briefing_is_never_launched_with_the_flag() {
        let mut rig = BriefingRig::new();
        rig.fixture.brigade.director_prompt = String::new();
        rig.fixture.brigade.goinkyo_consult = String::new();
        let argv = rig.launch(
            AgentKind::ClaudeCode,
            "absent",
            "director",
            BrigadeRole::Director,
        );
        assert!(
            !argv.contains(&"--append-system-prompt".to_string()),
            "{argv:?}"
        );
        assert!(!has_snapshot_off(&argv), "{argv:?}");
    }

    #[test]
    fn a_resumed_session_outside_a_brigade_is_never_launched_with_the_flag() {
        let rig = BriefingRig::new();
        let argv = launched_argv(open_embedded_cmd(
            AgentKind::ClaudeCode,
            "sess-1",
            rig.data_dir.path(),
        ));
        assert!(!has_snapshot_off(&argv), "{argv:?}");
    }

    #[test]
    fn a_codex_member_is_not_given_the_flag_whatever_its_transcript_says() {
        for id in ["", "codex-session"] {
            let rig = BriefingRig::new();
            rig.transcript("codex-session", &[snapshot_line("an older briefing")]);
            let argv = rig.launch(AgentKind::Codex, id, "director", BrigadeRole::Director);
            assert!(
                !argv.iter().any(|arg| arg.contains("snapshot")),
                "{id:?}: {argv:?}"
            );
        }
    }

    #[test]
    fn reading_the_codex_catalog_asks_the_binary_a_launch_would_use_and_hands_the_answer_over_as_it_is()
     {
        let fixture = SpawnFixture::new(PathBuf::from("/nonexistent"));
        let answered = ScriptedCatalog {
            answer: Ok("{\"models\":[]}".to_string()),
            asked: RefCell::new(Vec::new()),
        };
        let events = execute_cmd(
            Cmd::ReadCodexCatalog { round: 7 },
            &Deps {
                codex_catalog: &answered,
                ..fixture.deps()
            },
            &mut HashMap::new(),
            &mut Vec::new(),
        );
        assert_eq!(
            events,
            vec![Event::CodexCatalogRead {
                round: 7,
                result: Ok("{\"models\":[]}".to_string())
            }],
            "the answer carries the round it is for"
        );
        assert_eq!(
            *answered.asked.borrow(),
            ["codex"],
            "the bare name by default"
        );

        // The operator's own `[agent_binaries]` entry, as for a launch.
        let binaries = AgentBinaries {
            claude: None,
            codex: Some(PathBuf::from("C:/tools/codex.exe")),
        };
        let failing = ScriptedCatalog {
            answer: Err("timed out after 5s".to_string()),
            asked: RefCell::new(Vec::new()),
        };
        let events = execute_cmd(
            Cmd::ReadCodexCatalog { round: 8 },
            &Deps {
                codex_catalog: &failing,
                agent_binaries: &binaries,
                ..fixture.deps()
            },
            &mut HashMap::new(),
            &mut Vec::new(),
        );
        assert_eq!(
            events,
            vec![Event::CodexCatalogRead {
                round: 8,
                result: Err("timed out after 5s".to_string())
            }]
        );
        assert_eq!(*failing.asked.borrow(), ["C:/tools/codex.exe"]);
    }

    #[test]
    fn the_codex_trust_pane_strips_the_marker_too() {
        // Its own call site, reached by a different Cmd — the coverage the
        // first test buys does not extend to it for free.
        let fixture = SpawnFixture::new(PathBuf::from("/nonexistent"));
        let mut handles = HashMap::new();
        let mut discovery = Vec::new();

        execute_cmd(
            Cmd::OpenCodexTrustPane {
                key: SessionKey::from_id("trust"),
            },
            &fixture.deps(),
            &mut handles,
            &mut discovery,
        );

        assert_eq!(
            fixture.env_removes(),
            vec![
                STRIPPED_CHILD_ENV_VARS
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>()
            ]
        );
    }

    // --- sweeping the data directory --------------------------------------
    //
    // Temp directories and an in-memory store only: nothing here may name
    // the operator's real data directory.

    /// A data directory in a temp dir, with the working files of a store that
    /// has seen every way a member ends, all last written two hours ago:
    ///
    /// - brigade 1: a live Director and Worker, a Worker dismissed
    ///   (`dismiss_worker`, as the operator's and the exited-Worker dismissal
    ///   do) and a Goinkyo dismissed (`dismiss_goinkyo` from `_mcp` ends in the
    ///   same call);
    /// - brigade 2: disbanded (`delete_brigade`);
    /// - brigade 3: left with no member, removed by the janitor;
    /// - brigade 4: live, with a Goinkyo, and not staged by anything.
    struct SweepWorld {
        dir: tempfile::TempDir,
        data_dir: DataDir,
        store: RefCell<Store>,
    }

    const TWO_HOURS: Duration = Duration::from_secs(2 * 3600);

    /// `path` as if it had last been written `age` ago.
    fn written_ago(path: &Path, age: Duration) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(path).unwrap();
        file.set_modified(std::time::SystemTime::now() - age)
            .unwrap();
    }

    impl SweepWorld {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let data_dir = DataDir::new(dir.path().join("banto"));
            let mut store = Store::open_in_memory().unwrap();
            let b1 = store.create_brigade("one").unwrap();
            store
                .add_brigade_member(
                    b1,
                    "director",
                    BrigadeRole::Director,
                    Some(&SessionId("director-1".to_string())),
                )
                .unwrap();
            let live = store.add_worker(b1, None).unwrap();
            let dismissed = store.add_worker(b1, None).unwrap();
            store
                .add_brigade_member(b1, "goinkyo", BrigadeRole::Goinkyo, None)
                .unwrap();
            let b2 = store.create_brigade("two").unwrap();
            store
                .add_brigade_member(
                    b2,
                    "director",
                    BrigadeRole::Director,
                    Some(&SessionId("director-2".to_string())),
                )
                .unwrap();
            let b3 = store.create_brigade("three").unwrap();
            store
                .add_brigade_member(
                    b3,
                    "director",
                    BrigadeRole::Director,
                    Some(&SessionId("director-3".to_string())),
                )
                .unwrap();
            let b4 = store.create_brigade("four").unwrap();
            store
                .add_brigade_member(
                    b4,
                    "director",
                    BrigadeRole::Director,
                    Some(&SessionId("director-4".to_string())),
                )
                .unwrap();
            store
                .add_brigade_member(b4, "goinkyo", BrigadeRole::Goinkyo, None)
                .unwrap();
            assert_eq!(
                (b1, live.as_str(), dismissed.as_str()),
                (1, "worker-1", "worker-2")
            );
            assert_eq!((b2, b3, b4), (2, 3, 4));
            for (brigade, token) in [
                (1, "director"),
                (1, "worker-1"),
                (1, "worker-2"),
                (1, "goinkyo"),
                (2, "director"),
                (3, "director"),
                (4, "director"),
                (4, "goinkyo"),
            ] {
                written_ago(
                    &data_dir.member_config_path(brigade, token).unwrap(),
                    TWO_HOURS,
                );
            }
            for brigade in [1, 2, 3, 4] {
                written_ago(&data_dir.goinkyo_request_path(brigade).unwrap(), TWO_HOURS);
            }
            // Now the members end.
            assert!(store.dismiss_worker(1, "worker-2").unwrap());
            assert!(store.dismiss_worker(1, "goinkyo").unwrap());
            store.delete_brigade(2).unwrap();
            assert!(store.dismiss_worker(3, "director").unwrap());
            assert_eq!(store.delete_empty_brigades().unwrap(), 1);
            Self {
                dir,
                data_dir,
                store: RefCell::new(store),
            }
        }

        /// What is left of the working files: a member configuration by its file
        /// name, a consultation request as `request <file name>`; sorted.
        fn files(&self) -> Vec<String> {
            let mut left = Vec::new();
            for entry in std::fs::read_dir(self.data_dir.mcp_dir()).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_file() {
                    left.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
            for entry in std::fs::read_dir(self.data_dir.goinkyo_dir()).unwrap() {
                left.push(format!(
                    "request {}",
                    entry.unwrap().file_name().to_string_lossy()
                ));
            }
            left.sort();
            left
        }

        /// What the emporium of a process that resolved `db_path` and this data
        /// directory does on `Cmd::SweepDataDir`.
        fn sweep_as(&self, db_path: Option<PathBuf>, data_dir: Option<PathBuf>) -> Vec<Event> {
            let fixture = SpawnFixture::new(self.dir.path().join("claude"));
            let resolved = crate::startup::Resolved {
                config_file: crate::startup::ConfigFile::None,
                db_path,
                data_dir,
                claude_home: None,
                codex_home: None,
            };
            // The fixture's own store is empty; the sweep must read this world's.
            let deps = Deps {
                store: &self.store,
                child: Some(&resolved),
                ..fixture.deps()
            };
            execute_cmd(
                Cmd::SweepDataDir,
                &deps,
                &mut HashMap::new(),
                &mut Vec::new(),
            )
        }
    }

    const SURVIVORS: [&str; 5] = [
        "1-director.json",
        "1-worker-1.json",
        "4-director.json",
        "4-goinkyo.json",
        // Only a brigade that has a Goinkyo keeps its request: the dismissed
        // Goinkyo's (1), the disbanded (2) and the emptied (3) brigades' are gone.
        "request 4.txt",
    ];

    #[test]
    fn a_sweep_leaves_files_only_for_members_the_store_still_has() {
        let world = SweepWorld::new();
        let own = world.data_dir.clone();
        let events = world.sweep_as(Some(own.db_path()), Some(own.root().to_path_buf()));
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(world.files(), SURVIVORS);
    }

    #[test]
    fn a_member_the_emporium_is_not_showing_keeps_its_files() {
        // Brigade 4 is in the store and on no stage: the store is the truth, not
        // what this process happens to have open.
        let world = SweepWorld::new();
        let own = world.data_dir.clone();
        world.sweep_as(Some(own.db_path()), Some(own.root().to_path_buf()));
        assert!(
            world
                .data_dir
                .member_config_path(4, "goinkyo")
                .unwrap()
                .is_file()
        );
    }

    #[test]
    fn an_emporium_whose_store_is_not_the_directorys_own_deletes_nothing() {
        // A test profile (`db_path` elsewhere) shares the data directory with the
        // everyday banto but not its store: to it, the everyday one's files have
        // no owner. Every way of not being sure leaves everything where it is.
        let world = SweepWorld::new();
        let all = world.files();
        let own = world.data_dir.clone();
        let root = Some(own.root().to_path_buf());
        for (label, db_path, data_dir) in [
            (
                "another database",
                Some(world.dir.path().join("test profile.db")),
                root.clone(),
            ),
            (
                "the database beside, not in, the directory",
                Some(own.root().parent().unwrap().join("banto.db")),
                root.clone(),
            ),
            ("no database known", None, root.clone()),
            ("no data directory known", Some(own.db_path()), None),
            (
                "a relative data directory and database",
                Some(PathBuf::from("banto.db")),
                Some(PathBuf::new()),
            ),
        ] {
            let events = world.sweep_as(db_path, data_dir);
            assert!(events.is_empty(), "{label}: {events:?}");
            assert_eq!(world.files(), all, "{label}");
        }
        // With no hand-off at all (only a test has none), nothing either.
        let fixture = SpawnFixture::new(world.dir.path().join("claude"));
        let deps = Deps {
            store: &world.store,
            child: None,
            exe: crate::exe::test_support::current(),
            ..fixture.deps()
        };
        execute_cmd(
            Cmd::SweepDataDir,
            &deps,
            &mut HashMap::new(),
            &mut Vec::new(),
        );
        assert_eq!(world.files(), all);
        // And the directory's own database does sweep.
        world.sweep_as(Some(own.db_path()), root);
        assert_eq!(world.files(), SURVIVORS);
    }

    #[test]
    fn files_written_inside_the_grace_survive_whatever_the_store_says() {
        // A request is written before its row; a config's name is reused (the
        // Goinkyo's) by a member about to start. Neither has an owner yet.
        let world = SweepWorld::new();
        let own = world.data_dir.clone();
        let young = Duration::from_secs(60);
        written_ago(&own.goinkyo_request_path(1).unwrap(), young);
        written_ago(&own.member_config_path(1, "worker-2").unwrap(), young);
        world.sweep_as(Some(own.db_path()), Some(own.root().to_path_buf()));
        let files = world.files();
        assert!(files.contains(&"request 1.txt".to_string()), "{files:?}");
        assert!(files.contains(&"1-worker-2.json".to_string()), "{files:?}");
        // Brigade 2's are still old orphans.
        assert!(!files.contains(&"2-director.json".to_string()), "{files:?}");
        assert!(!files.contains(&"request 2.txt".to_string()), "{files:?}");
    }

    #[test]
    fn files_banto_did_not_name_are_never_deleted() {
        let world = SweepWorld::new();
        let own = world.data_dir.clone();
        let others = [
            own.mcp_dir().join("notes.txt"),
            own.mcp_dir().join("7-evil.json"),
            own.mcp_dir().join("9-worker-1.json.bak"),
            own.mcp_dir().join("07-worker-1.json"),
            own.mcp_dir().join("7-worker-01.json"),
            own.mcp_dir().join("7-Worker-1.json"),
            own.mcp_dir().join("8-worker-1.JSON"),
            own.goinkyo_dir().join("9.json"),
            own.root().join("banto.db"),
            own.logs_dir().join("brigade-9.log"),
        ];
        for other in &others {
            written_ago(other, TWO_HOURS);
        }
        world.sweep_as(Some(own.db_path()), Some(own.root().to_path_buf()));
        for kept in &others {
            assert!(kept.is_file(), "{} was deleted", kept.display());
        }
    }

    #[test]
    fn a_sweep_of_a_data_directory_that_does_not_exist_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = DataDir::new(dir.path().join("never made"));
        let store = RefCell::new(Store::open_in_memory().unwrap());
        sweep_data_dir_with(&data_dir, &store, std::time::SystemTime::now());
        assert!(!data_dir.root().exists());
    }

    #[test]
    fn a_member_that_cannot_be_given_a_configuration_file_is_not_wired() {
        // A hand-edited store can hold any token; none gets a file the sweep could
        // not tell from a stranger's.
        let dir = tempfile::tempdir().unwrap();
        let data_dir = DataDir::new(dir.path().join("banto"));
        let err = write_mcp_config(
            &data_dir,
            Path::new("banto"),
            1,
            "evil",
            BrigadeRole::Worker,
            None,
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("evil"), "{err}");
        assert!(!data_dir.root().exists());
        let path = write_mcp_config(
            &data_dir,
            Path::new("banto"),
            1,
            "worker-3",
            BrigadeRole::Worker,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(Some(path), data_dir.member_config_path(1, "worker-3"));
    }
}

#[cfg(test)]
mod build_commit_tests {
    /// `unknown` is the fallback `build.rs` writes when git cannot say; any
    /// other value is a commit id. Either way the start line has something to print.
    #[test]
    fn the_build_commit_is_never_empty() {
        assert!(!env!("BANTO_BUILD_COMMIT").is_empty());
    }

    /// The flag is one of three words whatever git said, so the start line never
    /// carries `dirty=` with nothing after it.
    #[test]
    fn the_build_dirty_flag_is_never_empty() {
        assert!(["yes", "no", "unknown"].contains(&env!("BANTO_BUILD_DIRTY")));
    }

    #[test]
    fn this_builds_start_line_carries_the_dirty_flag_after_the_commit() {
        let line = banto_core::pane_log::start_line(
            env!("CARGO_PKG_VERSION"),
            env!("BANTO_BUILD_COMMIT"),
            env!("BANTO_BUILD_DIRTY"),
            1,
            1,
        )
        .render(0, 0);
        let expected = format!(
            "commit={} dirty={} schema=",
            env!("BANTO_BUILD_COMMIT"),
            env!("BANTO_BUILD_DIRTY")
        );
        assert!(line.contains(&expected), "{line}");
        println!("{line}");
    }
}
