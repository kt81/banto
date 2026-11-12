//! The "emporium" (大店 / `--emporium` / `--oodana`) mode: banto as a
//! persistent left sidebar (the session list) plus a pane area beside it that
//! hosts the staged session or a brigade's tiles, embedded.
//!
//! Per `docs/DISCIPLINE.md` §4, this module is a thin **shell**: it gathers
//! facts about the outside world into
//! [`engine::Event`]s, calls the pure [`engine::update`], and executes the
//! [`engine::Cmd`]s it returns — process spawning, PTY reads/writes, store
//! reads/writes, and drawing all live here; none of the *decisions* do (see
//! `super::engine`, which owns `Stage`/`Focus`/the relay engine/etc.).
//!
//! The chōba list TUI (`crate::tui`) owns the shared pieces this reuses —
//! `App` (list state), the `view` renderers, the store-load helpers, and
//! `render_modal`. It has its own, separate event loop and is untouched by
//! this migration.
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
use ratatui::text::Span;
use ratatui::widgets::{Block, Paragraph};

use banto_core::app::{App, Mode};
use banto_core::config::{AgentBinaries, BrigadeConfig, KeysConfig, ResolvedAgents};
use banto_core::engine::{
    self, Axis, BrigadeNameFacts, ButtonKind, Cmd, EmporiumState, Event, Focus, Ghost,
    GoinkyoObservation, GoinkyoSpawnCandidate, GroupJoinTargetData, PrefixKey, RelayObservation,
    SessionKey, Stage, StoreIntent,
};
use banto_core::input::InputEvent;
use banto_core::layout::title_text;
use banto_core::model::{
    AgentKind, BrigadeId, BrigadeRole, DIRECTOR_TOKEN, MemberGlance, MemberToken, MessageGlance,
    SessionId, SessionToOpen,
};
use banto_core::replay::{STREAM_VERSION, TimedEvent};
use banto_core::screen::Screen;
use banto_core::status::AgeThresholds;
use banto_io::claude_home::ClaudeHome;
use banto_io::codex_activity;
use banto_io::codex_home::CodexHome;
use banto_io::codex_liveness::SysinfoStartTime;
use banto_io::input::{CrosstermInputReader, InputReader, StampedEvent};
use banto_io::provider::SessionProvider;
use banto_io::provider::cache::MetaCache;
use banto_io::provider::claude_code::ClaudeCodeProvider;
use banto_io::pty::{PortablePtyHost, PtyHost, STRIPPED_CHILD_ENV_VARS};
use banto_io::status::{
    LIVE_STATUS_BUSY, LIVE_STATUS_WAITING, LiveSession, SysinfoProbe, ancestry_reaches,
    read_live_sessions,
};
use banto_io::store::Store;
use banto_tui::paint;
use banto_tui::strip::{StripData, StripMessage, render_strip};
use banto_tui::view::{self, WAITING_ACTIVITY_COLOR};

use crate::opener;
use crate::session;
use crate::tui::LiveWatch;

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
/// past clippy's `too_many_arguments` limit — mirrors `crate::opener::OpenContext`'s
/// role for the same problem there.
pub struct EmporiumSettings<'a> {
    /// `[brigade]`: how many fresh Workers `B` auto-spawns when forming a
    /// new brigade, the `--model` an auto-spawned Worker launches with, and
    /// whether the relay engine is enabled.
    pub brigade: &'a BrigadeConfig,
    /// `[keys]`: the tmux-style prefix chord for pane operations.
    pub keys: &'a KeysConfig,
    /// `[agent_binaries]` — see `crate::opener::agent_binary`.
    pub agent_binaries: &'a AgentBinaries,
    /// `Config.agents`, resolved — see `crate::tui::Context::enabled_agents`.
    /// The whole [`ResolvedAgents`], not just its `enabled` set, so [`run`]
    /// can also post the startup notice for a name it had to ignore (see
    /// `session::agents_ignored_notice`).
    pub resolved_agents: &'a ResolvedAgents,
}

/// Run the emporium mode until the user quits (`q`/Esc from the sidebar; a
/// second press while sessions are running).
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

    // Same lifetime and the same reason as `superseded_failed` below: created
    // once here so the bootstrap walk's parses are already in it when the
    // loop's first `gather_reload` runs a second later.
    let meta_cache = MetaCache::new();
    let metas = session::discover_all(claude_home, codex_home, enabled_agents, Some(&meta_cache))?;
    // In-memory only, for this process's lifetime — see
    // `crate::tui::superseded_from_metas`'s doc. Created once here and
    // threaded through every reload (the bootstrap below and every later
    // `gather_reload`) rather than per-call, so a permanently-unresolvable
    // continuation is scanned at most once per banto run.
    let superseded_failed = RefCell::new(HashSet::new());
    // Same store-backed state the chōba list builds, so grouping / pins /
    // archived-hiding / brigade hiding show identically in the sidebar. This
    // one-time bootstrap stays outside `update`: `App::with_*` are
    // construction-only builders, not a repeating decision.
    let (rows, pinned, groups, session_groups, hidden, directors, superseded) = {
        let store = store.borrow();
        let superseded = crate::tui::superseded_from_metas(&metas, &store, &superseded_failed);
        let rows = session::rows_from_metas(metas, claude_home, codex_home, thresholds);
        let rows = crate::tui::mark_archived(rows, &store);
        let pinned = crate::tui::load_pinned(&store);
        let groups = crate::tui::load_groups(&store);
        let session_groups = crate::tui::load_session_groups(&store, &groups);
        let hidden = crate::tui::load_hidden_member_ids(&store);
        let directors = crate::tui::load_directors(&store);
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
        .with_hidden_member_ids(hidden)
        .with_directors(directors)
        .with_superseded(superseded)
        // Two lines per row (title/age, then cwd/agent) — the sidebar has
        // the width for markers and a title but not a cwd too (see
        // `banto_tui::view`'s module doc's "Row layout" section); the chōba
        // stays the default of 1.
        .with_lines_per_row(2)
        .with_enabled_agents(enabled_agents.clone());
    // The trust notice wins the status line when both apply: an unknown agent
    // name is a typo in a setting, while an untrusted hook is a cell about to
    // form with members nothing will ever brief.
    let trust_notice = codex_home
        .filter(|_| enabled_agents.contains(&AgentKind::Codex))
        .and_then(|home| {
            let has_brigade = !store
                .borrow()
                .list_brigades()
                .unwrap_or_default()
                .is_empty();
            // An unknowable executable path counts as launchable: blaming a
            // space nothing has established is there would be worse than
            // leaving the trust question to speak for itself.
            let hook_launchable = match std::env::current_exe() {
                Ok(exe) => opener::hook_command_is_launchable(&opener::forward_slash_path(&exe)),
                Err(_) => true,
            };
            session::codex_trust_notice(
                banto_io::codex_trust::hook_trust_state(home),
                true,
                has_brigade,
                hook_launchable,
            )
        });
    if let Some(notice) =
        trust_notice.or_else(|| session::agents_ignored_notice(settings.resolved_agents))
    {
        app.set_status(notice, Instant::now());
    }

    let deps = Deps {
        claude_home,
        codex_home,
        thresholds,
        store,
        superseded_failed: &superseded_failed,
        meta_cache: &meta_cache,
        pty_host: &PortablePtyHost,
        input_reader: &CrosstermInputReader,
        brigade,
        agent_binaries,
        enabled_agents,
    };
    let (mut terminal, mut frame_output) = setup_terminal()?;
    let result = event_loop(&mut terminal, &mut frame_output, &mut app, &deps, keys);
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
    since: SystemTime,
    member: Option<(BrigadeId, MemberToken)>,
    /// The spawned child's pid, when the platform reports one — the exact
    /// match against `sessions/<pid>.json` that [`poll_discovery`] tries
    /// before falling back to scanning session files by cwd.
    pid: Option<u32>,
    /// Which product this is — Claude's own two sources above don't apply
    /// to a Codex child at all (see [`poll_discovery`]'s doc for the
    /// store-based fallback this gates), and only a Codex tracker ever
    /// gives up on a [`CODEX_WORKER_DISCOVERY_TIMEOUT`].
    agent: AgentKind,
    /// Whether [`poll_discovery`] has already told the operator this Claude
    /// tracker looks stuck behind an unanswered directory-trust prompt —
    /// set the first time `claude_directory_trust` reports anything but
    /// `Trusted`, so the status line states it once instead of restamping
    /// the same notice every poll (~every loop iteration, not just once a
    /// second). The Codex-side equivalent (`PendingKickoff::notified_untrusted`)
    /// lives in `engine.rs` instead, because that one also gates an actual
    /// keystroke and needs the core's own quiet-period timing; this one only
    /// ever gates a notice, and this tracker — already the shell's own
    /// discovery bookkeeping, never seen by the core (see this struct's own
    /// doc) — is exactly where that one bit belongs.
    notified_untrusted: bool,
}

/// How often the relay engine (and the pending-submit flush / status expiry
/// bundled into the same [`Event::Tick`]) re-evaluates.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Read-only shell dependencies that don't change across an [`event_loop`]
/// iteration — bundled (mirroring `crate::tui::Context`'s role in the
/// chōba list) so `event_loop`/`execute_cmd`/`gather_reload`'s argument
/// lists don't keep growing one-by-one as more reload-path state (like
/// [`Self::superseded_failed`]) gets threaded through.
struct Deps<'a> {
    claude_home: &'a ClaudeHome,
    /// `None` degrades to no Codex sessions in the sidebar, not an error —
    /// same contract as `crate::tui::Context::codex_home`.
    codex_home: Option<&'a CodexHome>,
    thresholds: &'a AgeThresholds,
    store: &'a RefCell<Store>,
    /// See [`crate::tui::superseded_from_metas`]'s doc: in-memory only, for
    /// this process's lifetime.
    superseded_failed: &'a RefCell<HashSet<SessionId>>,
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
    /// `[agent_binaries]` from config.toml — see `opener::agent_binary`.
    agent_binaries: &'a AgentBinaries,
    /// `Config.agents`, resolved — see `crate::tui::Context::enabled_agents`.
    enabled_agents: &'a BTreeSet<AgentKind>,
}

fn event_loop(
    terminal: &mut Tui,
    frame_output: &mut FrameOutputHandle<Stdout>,
    app: &mut App,
    deps: &Deps,
    keys: &KeysConfig,
) -> Result<()> {
    let brigade = deps.brigade;
    let mut state = EmporiumState::new(PrefixKey::parse(&keys.prefix));
    let mut handles: HashMap<SessionKey, PtyHandle> = HashMap::new();
    let mut discovery: Vec<DiscoveryTracker> = Vec::new();
    let mut watch = LiveWatch::new(deps.claude_home, deps.codex_home);
    let provider = ClaudeCodeProvider::new(deps.claude_home.clone());
    let mut last_tick: Option<Instant> = None;
    let mut glance_gate = GlanceGate::default();
    // The stored sidebar width, fed as the very first event of the very first
    // iteration (see the loop) so it is part of the recorded stream.
    let mut stored_sidebar_width = Some(Event::SidebarWidthLoaded {
        width: deps.store.borrow().sidebar_width().ok().flatten(),
    });
    // Opened once, off this thread — see `banto_io::input`'s module doc for
    // why the read no longer happens inline here: a slow iteration
    // (rendering, a reload pass, ...) between this loop getting back around
    // to receiving and an event's true arrival must never inflate
    // `paste_acc`'s gap timing.
    let input_rx = deps.input_reader.open();
    let mut input_log = open_input_log();
    let mut stage_log = open_stage_log();
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
        // `PtyPoll::Disconnected` is only ever reached once a handle's every
        // last chunk has been drained (see `PtyHandle::poll`'s doc), so a
        // handle reports its end once, right here, then gets dropped below —
        // as an exit or as lost output, whichever the evidence supports.
        let pty_pump_start = Instant::now();
        for (key, handle) in &mut handles {
            loop {
                match handle.poll() {
                    PtyPoll::Chunk(chunk) => events.push_back(Event::PtyOutput {
                        key: key.clone(),
                        chunk,
                    }),
                    PtyPoll::Empty => break,
                    PtyPoll::Disconnected => {
                        // Two facts, reported apart (`disconnect_event`): core
                        // decides what each means.
                        events.push_back(super::session::disconnect_event(
                            key.clone(),
                            handle.exit_reason(),
                        ));
                        break;
                    }
                }
            }
        }
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
            let live = read_live_sessions(&deps.claude_home.sessions_dir());
            events.extend(poll_discovery(
                &mut discovery,
                &provider,
                &claimed,
                &live,
                deps.store,
                deps.claude_home,
            ));
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
            events.extend(gather_fork_observations(
                &state,
                deps.store,
                deps.claude_home,
                &handles,
                app,
                Some(&mut tick_timings),
            ));
            let relay = gather_relay_observations(
                &state,
                deps.store,
                deps.claude_home,
                deps.codex_home,
                app,
                Some(&mut tick_timings),
                &mut glance_gate,
            );
            let brigades = std::mem::take(&mut glance_gate.brigades);
            events.push_back(Event::Tick { relay, brigades });
            let observation =
                gather_goinkyo_observation(&state, deps.store, app, Some(&mut tick_timings));
            events.push_back(Event::GoinkyoAwaitingSpawn { observation });
        }
        let tick_dur = tick_start.elapsed();

        // Drain the event queue through `update`, executing the `Cmd`s it
        // returns and feeding any follow-up fact back onto the same queue —
        // the synchronous relaxation (DISCIPLINE §6.1) generalized from just
        // the store to every shell-executed `Cmd` (spawning a PTY child is
        // just as synchronous a call as a store write).
        let dispatch_start = Instant::now();
        while let Some(ev) = events.pop_front() {
            if let Some(recorder) = &mut event_recorder {
                recorder.record(&ev, now);
            }
            let cmds = engine::update(&mut state, app, brigade, ev, now);
            for cmd in cmds {
                events.extend(execute_cmd(cmd, deps, &mut handles, &mut discovery));
            }
        }
        let dispatch_dur = dispatch_start.elapsed();

        // A `PtyExited` or `PtyOutputLost` handler drops the session's `Screen`;
        // the handle itself (now pointing at a dead reader thread) is reaped here.
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
        let reaped: Vec<PtyHandle> = handles
            .extract_if(|key, _| !state.screens.contains_key(key))
            .map(|(_, handle)| handle)
            .collect();
        if !reaped.is_empty() {
            std::thread::spawn(move || drop(reaped));
        }

        let draw_start = Instant::now();
        let mut draw_timings = DrawTimings::default();
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

/// Windows gives a console-closing child ~5s (`CTRL_CLOSE_EVENT`) before it
/// force-terminates the process — the budget `shutdown_handles` mirrors for
/// its whole sweep (see that function's doc for why it's shared, not per
/// child).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
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
/// Deliberately NOT how the prefix-x kill path (`Cmd::KillPty`) works: that
/// is explicit user intent for an immediate stop, and session jsonl is
/// append-only and parsed leniently on both sides, so a hard kill there
/// risks at most one truncated trailing line — an already-accepted trade for
/// responsiveness that this sweep does not need to make, since nothing here
/// is time-sensitive to the user.
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
        Cmd::ResizePty { key, rows, cols } => {
            if let Some(handle) = handles.get_mut(&key) {
                handle.resize(rows, cols);
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
        } => execute_open_embedded(
            OpenEmbeddedRequest {
                key,
                target,
                brigade,
                model,
                effort,
                permission_mode,
                disallowed_tools,
            },
            deps,
            handles,
            discovery,
        ),
        Cmd::CheckCodexTrust => execute_check_codex_trust(deps),
        Cmd::OpenCodexTrustPane { key } => execute_open_codex_trust_pane(key, deps, handles),
        Cmd::CheckWorkerDirectoryTrust { key, cwd } => {
            execute_check_worker_directory_trust(key, &cwd, deps)
        }
        Cmd::CheckGoinkyoDirectoryTrust { key, cwd } => {
            execute_check_goinkyo_directory_trust(key, &cwd, deps)
        }
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
        Cmd::RingHostBell => {
            // Like OSC 52 above, BEL changes neither cells nor cursor, so
            // emitting it before draw cannot desynchronize ratatui's model.
            // It is deliberately a fixed byte, not child-controlled payload.
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(b"\x07").and_then(|()| stdout.flush());
            Vec::new()
        }
    }
}

/// Writes a bare OSC 8 close (`ESC ] 8 ; ; ESC \`) into the current frame
/// output. It is emitted only after a presentation that queued screen bytes;
/// a skipped frame sends nothing at all. Split out so its exact bytes and the
/// flush boundary are independently testable without a real terminal.
fn close_any_open_hyperlink<W: io::Write>(w: &mut W) -> io::Result<()> {
    w.write_all(b"\x1b]8;;\x1b\\")?;
    w.flush()
}

/// Read whether banto's `SessionStart` hook looks trusted right now —
/// freshly, not cached, since the operator may have approved it in a pane
/// since this run started — and whether it could even fire from this
/// executable's path. `deps.codex_home` absent (Codex unresolved) reads as
/// unprimed: `Cmd::CheckCodexTrust` is only ever issued once a brigade
/// formation already resolved to a Codex Worker, so "can't tell" must not
/// read as "trusted".
fn execute_check_codex_trust(deps: &Deps) -> Vec<Event> {
    let primed = deps.codex_home.is_some_and(|home| {
        banto_io::codex_trust::hook_trust_state(home)
            == banto_io::codex_trust::HookTrustState::Primed
    });
    let hook_launchable = match std::env::current_exe() {
        Ok(exe) => opener::hook_command_is_launchable(&opener::forward_slash_path(&exe)),
        Err(_) => true,
    };
    vec![Event::CodexTrustChecked {
        primed,
        hook_launchable,
    }]
}

/// Read whether Codex has been told to trust `cwd` — freshly, not cached,
/// since the operator may answer its prompt in the pane at any moment while
/// `engine::update_tick` keeps re-asking. `deps.codex_home` absent reads as
/// untrusted (`false`), the same "can't tell must not read as safe" rule
/// [`execute_check_codex_trust`] follows: this Cmd only exists to gate
/// typing into a Codex pane, so an unknowable answer must not accidentally
/// permit it.
fn execute_check_worker_directory_trust(
    key: SessionKey,
    cwd: &std::path::Path,
    deps: &Deps,
) -> Vec<Event> {
    let trusted = deps.codex_home.is_some_and(|home| {
        banto_io::directory_trust::codex_directory_trust(home, cwd)
            == banto_io::directory_trust::DirectoryTrust::Trusted
    });
    vec![Event::WorkerDirectoryTrustChecked { key, trusted }]
}

/// Same shape as [`execute_check_worker_directory_trust`], but reads
/// Claude's own trust registry (`banto_io::directory_trust::claude_directory_trust`)
/// instead of Codex's — a different underlying file, not just a different
/// product label; `deps.claude_home` is never optional the way
/// `deps.codex_home` is, so there is no "product absent" case to fold in
/// here.
fn execute_check_goinkyo_directory_trust(
    key: SessionKey,
    cwd: &std::path::Path,
    deps: &Deps,
) -> Vec<Event> {
    let trusted = banto_io::directory_trust::claude_directory_trust(deps.claude_home, cwd)
        == banto_io::directory_trust::DirectoryTrust::Trusted;
    vec![Event::GoinkyoDirectoryTrustChecked { key, trusted }]
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
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            return vec![Event::SpawnFailed {
                key,
                error: err.to_string(),
            }];
        }
    };
    let argv = crate::codex_trust::trust_argv(&exe, deps.agent_binaries);
    let cwd = std::env::current_dir().ok();
    match PtyHandle::open(
        deps.pty_host,
        &argv,
        cwd.as_deref(),
        &[],
        STRIPPED_CHILD_ENV_VARS,
        24,
        80,
    ) {
        Ok(handle) => {
            handles.insert(key.clone(), handle);
            vec![Event::Spawned { key }]
        }
        Err(err) => vec![Event::SpawnFailed {
            key,
            error: err.to_string(),
        }],
    }
}

/// Build the [`opener::AgentLaunch`] for opening `target`: resuming it via
/// [`opener::decide_inplace_resume`] when a real id is already known (`None`
/// if a live pane elsewhere refuses the resume), or a fresh unresumed launch
/// otherwise — either way with `model`/`briefing` carried on the result.
/// `target.agent` picks the variant; `briefing` only ever reaches the
/// `Claude` one, because only Claude takes a briefing as launch argv; a
/// Codex member gets the same text from the `banto _hook` process the
/// injected `SessionStart` hook spawns
/// (docs/notes/codex-briefing-spike.md). `--model` applies the same way to a resume as to a
/// fresh spawn: a Worker resumed without it falls back to the operator's
/// own default model instead of the brigade's configured one
/// (`engine::stage_brigade` never sets it for the Director, so this is
/// never reached for one).
///
/// `briefing` is the member's already-rendered role briefing
/// (`--append-system-prompt`, see [`crate::briefing::render`]) — rendered by
/// the caller because it needs a store read for the roster, verified against
/// `claude` 2.1.219 to apply to a `--resume` exactly as it does to a fresh
/// launch, which is what makes it reach a resumed Director at all.
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
    permission_mode: Option<&str>,
    disallowed_tools: Option<&str>,
    briefing: Option<&str>,
    ctx: &opener::OpenContext,
) -> Option<opener::AgentLaunch> {
    let resume = if target.id.is_empty() {
        None
    } else {
        // Called for its refusal, not its value: this is the no-double-resume
        // guard (AGENTS.md invariant 4), and `?` is what turns a session that
        // is already live somewhere else into `None`. The `InPlaceLaunch` it
        // hands back is dropped because its argv is by construction
        // `inplace_argv(target.agent, Some(&target.id), ...)` — the same id
        // we resume below.
        opener::decide_inplace_resume(target, ctx)?;
        Some(target.id.clone())
    };
    Some(match target.agent {
        AgentKind::ClaudeCode => opener::AgentLaunch::Claude {
            resume,
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            permission_mode: permission_mode.map(str::to_string),
            disallowed_tools: disallowed_tools.map(str::to_string),
            append_system_prompt: briefing.map(str::to_string),
            mcp_config: None,
        },
        // `briefing` is deliberately unused here: Codex has no
        // `--append-system-prompt`, so a Codex member's briefing is rendered
        // by the `banto _hook` process the injected SessionStart hook spawns,
        // not passed on the argv. `brigade` is left `None` for the caller to
        // fill for the same reason `mcp_config` is — it needs the running
        // executable's own path, which is I/O this stays free of. `effort`,
        // `permission_mode`, and `disallowed_tools` are all dropped for the
        // same reason `AgentLaunch::Codex` carries no field for any of them
        // — a Goinkyo (the only source of any today) is Claude-only, so
        // this arm never actually receives one; see that variant's own doc.
        AgentKind::Codex => opener::AgentLaunch::Codex {
            resume,
            model: model.map(str::to_string),
            cwd: target.cwd.clone(),
            brigade: None,
        },
    })
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
}

/// Spawn `target` under `key`, enforcing the no-double-resume guard for a
/// known (non-empty) id — reusing the chōba in-place decision — or
/// skipping it entirely for a fresh (empty-id) spawn, which has no existing
/// session to double-resume. `brigade` wires the launch to banto's own MCP
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
    } = request;
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
    let open_ctx = opener::OpenContext {
        probe: &SysinfoProbe,
        live: &live,
        binaries: deps.agent_binaries,
        codex_home: deps.codex_home,
        start_time: &SysinfoStartTime,
    };
    let Some(mut launch) = build_open_launch(
        &target,
        model.as_deref(),
        effort.as_deref(),
        permission_mode.as_deref(),
        disallowed_tools.as_deref(),
        briefing.as_deref(),
        &open_ctx,
    ) else {
        return vec![Event::SpawnFailed {
            key,
            error: "already running elsewhere".to_string(),
        }];
    };
    // Each product reaches banto's own MCP server a different way: Claude via
    // a config file named on the argv, Codex via `-c` overrides built from
    // banto's executable path. Either can fail (a full disk, a moved
    // executable, ...) — refused outright rather than spawned wireless (see
    // this function's own doc for why).
    if let Some((brigade_id, token, role)) = &brigade {
        let known_id = (!target.id.is_empty()).then_some(target.id.as_str());
        let wiring = match &mut launch {
            opener::AgentLaunch::Claude { mcp_config, .. } => {
                write_mcp_config(*brigade_id, token, *role, known_id).map(|path| {
                    *mcp_config = Some(path);
                })
            }
            opener::AgentLaunch::Codex { brigade: slot, .. } => std::env::current_exe()
                .map_err(anyhow::Error::from)
                .map(|exe| {
                    *slot = Some(opener::CodexBrigade {
                        exe,
                        brigade_id: *brigade_id,
                        token: token.clone(),
                        role: *role,
                        session: known_id.map(str::to_string),
                    });
                }),
        };
        if let Err(err) = wiring {
            return vec![Event::SpawnFailed {
                key,
                error: format!("brigade channel wiring failed: {err}"),
            }];
        }
    }
    let argv = launch.argv(&opener::agent_binary(target.agent, deps.agent_binaries));
    let env = brigade_env(brigade.as_ref());
    // Size is corrected on this same tick's resize pass, once staged.
    match PtyHandle::open(
        deps.pty_host,
        &argv,
        Some(&target.cwd),
        &env,
        STRIPPED_CHILD_ENV_VARS,
        24,
        80,
    ) {
        Ok(handle) => {
            let pid = handle.pid();
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
                    cwd: target.cwd,
                    since: SystemTime::now(),
                    member: brigade.map(|(brigade_id, token, _)| (brigade_id, token)),
                    pid,
                    notified_untrusted: false,
                });
            }
            vec![Event::Spawned { key }]
        }
        Err(err) => vec![Event::SpawnFailed {
            key,
            error: err.to_string(),
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
            // `&mut` (not `&`): healing a member below persists a moved
            // session_id (`set_member_session`), not just reads one.
            let mut store = store.borrow_mut();
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
            let members = membership
                .as_ref()
                .zip(roster)
                .map(|((brigade_id, _, _), roster)| {
                    roster
                        .into_iter()
                        .map(|member| {
                            let healed_id = heal_member_session(
                                &mut store,
                                *brigade_id,
                                &member.token,
                                member.session_id,
                            );
                            (member.token, member.role, healed_id)
                        })
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
        StoreIntent::FormBrigade {
            director_row_id,
            director_agent,
            name,
            cwd,
            worker_count,
            worker_agent,
            worker_model,
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
                result,
            }]
        }
        StoreIntent::AddWorker {
            brigade_id,
            cwd,
            worker_agent,
            worker_model,
        } => {
            let result = add_worker_store(store, brigade_id, worker_agent);
            vec![Event::WorkerAdded {
                brigade_id,
                cwd,
                worker_agent,
                worker_model,
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
                        crate::tui::load_hidden_member_ids(&store),
                        crate::tui::load_directors(&store),
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
                        crate::tui::load_hidden_member_ids(&store),
                        crate::tui::load_directors(&store),
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
                        crate::tui::load_hidden_member_ids(&store),
                        crate::tui::load_directors(&store),
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
                hidden: crate::tui::load_hidden_member_ids(&store),
                directors: crate::tui::load_directors(&store),
            }]
        }
        StoreIntent::ClearMemberSession { brigade_id, token } => {
            let mut store = store.borrow_mut();
            let _ = store.clear_member_session(brigade_id, &token);
            // Same refresh `SetMemberSession` answers with: this member's id
            // just left (or, if it had none, stayed out of) the hidden set,
            // so the sidebar's own filter needs to catch up either way.
            vec![Event::MemberSessionRecorded {
                hidden: crate::tui::load_hidden_member_ids(&store),
                directors: crate::tui::load_directors(&store),
            }]
        }
    }
}

/// Follow `session_id` to its newest known auto-compaction continuation
/// (`Store::lineage_leaf`) and, if it moved, persist the move
/// (`Store::set_member_session`, v9 move semantics: any other row
/// holding the healed id is cleared) — closing the zombie loop for forks the
/// live watcher missed (banto wasn't running when they happened), so
/// re-staging resumes the true continuation instead of a stale ancestor.
/// `None` in, `None` out: a member still awaiting discovery has nothing to
/// heal. Tolerant: a lookup/write failure just leaves the id as recorded,
/// rather than blocking membership resolution over it.
fn heal_member_session(
    store: &mut Store,
    brigade_id: BrigadeId,
    token: &str,
    session_id: Option<SessionId>,
) -> Option<String> {
    let recorded = session_id?;
    let leaf = store
        .lineage_leaf(&recorded)
        .unwrap_or_else(|_| recorded.clone());
    if leaf != recorded {
        let _ = store.set_member_session(brigade_id, token, &leaf);
    }
    Some(leaf.0)
}

/// Create the brigade, its Director row, and `worker_count` Worker rows
/// (schema v7). Not one shared transaction — each `add_brigade_member` call
/// commits on its own (see that method's own doc) — so a failure partway
/// through the Worker loop leaves the brigade, Director, and however many
/// Worker rows already succeeded in place; this only stops issuing the
/// remaining ones and reports the failure, rather than silently skipping
/// past it and reporting a partial success as if it were complete. Rare
/// enough (SQLite serializes writers and `Store::open`'s 5s busy_timeout
/// absorbs ordinary contention between this store's several writer
/// processes — a mid-loop failure here means that timeout ran out) that
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
    let brigade_id = store.create_brigade(name).map_err(|err| err.to_string())?;
    store
        .add_brigade_member_with_agent(
            brigade_id,
            DIRECTOR_TOKEN,
            BrigadeRole::Director,
            Some(&SessionId(director_row_id.to_string())),
            Some(director_agent),
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
/// over a transient filesystem hiccup. Also spends this reload's
/// lineage-resolution budget against the same discover() pass (see
/// [`crate::tui::superseded_from_metas`]).
fn gather_reload(deps: &Deps) -> Vec<Event> {
    let Ok(metas) = session::discover_all(
        deps.claude_home,
        deps.codex_home,
        deps.enabled_agents,
        Some(deps.meta_cache),
    ) else {
        return Vec::new();
    };
    let store = deps.store.borrow();
    let superseded = crate::tui::superseded_from_metas(&metas, &store, deps.superseded_failed);
    let rows = session::rows_from_metas(metas, deps.claude_home, deps.codex_home, deps.thresholds);
    let rows = crate::tui::mark_archived(rows, &store);
    let hidden = crate::tui::load_hidden_member_ids(&store);
    let directors = crate::tui::load_directors(&store);
    vec![Event::RowsLoaded {
        rows,
        hidden,
        directors,
        superseded,
    }]
}

/// How long a Codex-sourced discovery tracker waits for
/// `BrigadeMember::briefed_session_id` before giving up — a judgment call,
/// not a measured minimum: the happy path (2026-08-02 investigation)
/// resolved within a few seconds of the kickoff's submitting `\r`, so this
/// is generous headroom for slower model latency or a heavier real briefing
/// prompt, not a tuned floor. Only ever applies to a tracker whose
/// `DiscoveryTracker::agent` is `AgentKind::Codex` — Claude's own trackers
/// keep waiting forever, unchanged from before this existed.
const CODEX_WORKER_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(90);

/// This member's `briefed_session_id`, if the store has one — the id
/// `banto _hook` recorded on stdin the last time it ran for this member
/// (`BrigadeMember::briefed_session_id`'s own doc, in `banto-core`). The
/// only discovery source that exists at all for a Codex Worker: unlike
/// Claude, nothing Codex writes to disk names a session before its first
/// turn (measured 2026-08-02 — no live-state file, no session file, no
/// `threads` row, nothing), so this is that same fact reaching the store
/// instead, once `crate::engine`'s kickoff mechanism has forced that first
/// turn to happen.
fn codex_briefed_session_id(
    store: &RefCell<Store>,
    brigade_id: BrigadeId,
    token: &str,
) -> Option<String> {
    store
        .borrow()
        .brigade_members(brigade_id)
        .ok()?
        .into_iter()
        .find(|member| member.token == token)?
        .briefed_session_id
        .map(|session_id| session_id.0)
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
///    1 and 2 don't apply to it at all — see that function's own doc.
///
/// A Codex tracker still unresolved past [`CODEX_WORKER_DISCOVERY_TIMEOUT`]
/// gives up: removed the same as a resolved one, but reported as
/// [`Event::CodexWorkerDiscoveryTimedOut`] instead of
/// [`Event::DiscoveryResult`], so the operator sees why rather than the
/// pane just silently never identifying itself. A Claude tracker never
/// times out — unchanged from before this existed.
///
/// A Claude tracker still unresolved (regardless of how long) whose own cwd
/// reads as definitively [`banto_io::directory_trust::DirectoryTrust::NotTrusted`]
/// reports [`Event::ClaudeWorkerDirectoryUntrusted`] once — this pane isn't
/// silent because anything is broken, it's sitting behind an unanswered
/// trust prompt the same as a fresh Codex Worker would, just with nothing
/// *here* to gate: `poll_discovery` itself never types into a pane — a
/// Worker's own first turn is always the operator's, and a Goinkyo's own
/// kickoff (which does type, once its own `Cmd::CheckGoinkyoDirectoryTrust`
/// answers trusted) already checks this same condition independently, on
/// its own schedule — so this report is only ever a silence to explain,
/// never an action to hold back. (Not true of banto as a whole: an
/// already-*discovered* Claude Worker's pane does get typed into, by the
/// relay nudge — but this paragraph is about a tracker that never got that
/// far.)
///
/// Deliberately narrower than Codex's own gate
/// ([`execute_check_worker_directory_trust`] collapses `NotTrusted` and
/// `Unknown` together, since an unknowable answer must not accidentally
/// permit a keystroke): `Unknown` just means no record exists yet, which is
/// the ordinary shape of "never opened this directory before" and would
/// fire on nearly every fresh Worker for the second or so before its first
/// launch even finishes settling — this only gates a notice, not an action,
/// so a false positive costs real annoyance with nothing gained. `NotTrusted`
/// alone is not a guess at "maybe still pending": a real `~/.claude.json`
/// on this machine has entries recording `hasTrustDialogAccepted: false`
/// independent of any `true` one, so a directory sitting at an unanswered
/// prompt is expected to read as `NotTrusted`, not `Unknown`, by the time
/// this ever fires.
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
    let mut timed_out: Vec<usize> = Vec::new();
    let mut newly_untrusted: Vec<usize> = Vec::new();
    for (i, tracker) in trackers.iter().enumerate() {
        let by_pid = tracker
            .pid
            .and_then(|pid| live_session_id(live, pid, &tracker.cwd));
        let id = by_pid
            .filter(|id| !claimed.contains(id) && !used_this_pass.contains(id))
            .or_else(|| {
                provider
                    .find_new_sessions(&tracker.cwd, tracker.since)
                    .into_iter()
                    .map(|id| id.0)
                    .find(|id| !claimed.contains(id) && !used_this_pass.contains(id))
            })
            .or_else(|| {
                (tracker.agent == AgentKind::Codex)
                    .then_some(tracker.member.as_ref())
                    .flatten()
                    .and_then(|(brigade_id, token)| {
                        codex_briefed_session_id(store, *brigade_id, token)
                    })
                    .filter(|id| !claimed.contains(id) && !used_this_pass.contains(id))
            });
        if let Some(id) = id {
            used_this_pass.insert(id.clone());
            resolved.push((i, id));
        } else if tracker.agent == AgentKind::Codex
            && SystemTime::now()
                .duration_since(tracker.since)
                .is_ok_and(|elapsed| elapsed >= CODEX_WORKER_DISCOVERY_TIMEOUT)
        {
            timed_out.push(i);
        } else if tracker.agent == AgentKind::ClaudeCode
            && !tracker.notified_untrusted
            && banto_io::directory_trust::claude_directory_trust(claude_home, &tracker.cwd)
                == banto_io::directory_trust::DirectoryTrust::NotTrusted
        {
            newly_untrusted.push(i);
        }
    }
    if resolved.is_empty() && timed_out.is_empty() && newly_untrusted.is_empty() {
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
    events.extend(timed_out.iter().filter_map(|&i| {
        trackers[i]
            .member
            .as_ref()
            .map(|(_, token)| Event::CodexWorkerDiscoveryTimedOut {
                key: trackers[i].key.clone(),
                token: token.clone(),
            })
    }));
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
    let mut removed_indices: HashSet<usize> = resolved.into_iter().map(|(i, _)| i).collect();
    removed_indices.extend(timed_out);
    let mut i = 0;
    trackers.retain(|_| {
        let keep = !removed_indices.contains(&i);
        i += 1;
        keep
    });
    events
}

/// The session id `claude` published for the process at `pid`, if that
/// live-state entry is really the child banto spawned into `cwd`.
///
/// The cwd check is the guard against a stale file: a `sessions/<pid>.json`
/// left behind by a session whose pid the OS has since recycled would
/// otherwise hand a Worker somebody else's session id, and banto would go
/// on to `--resume` it — the one thing it must never do twice. A mismatch
/// (or a file with no cwd recorded) simply declines, leaving the session-file
/// scan to answer.
fn live_session_id(live: &[LiveSession], pid: u32, cwd: &Path) -> Option<String> {
    live.iter()
        .find(|entry| entry.pid == pid && entry.cwd.as_deref() == Some(cwd))
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
/// The Claude arm excludes [`LIVE_STATUS_WAITING`] as well as
/// [`LIVE_STATUS_BUSY`] — see that constant's own doc for the measurement
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
fn gather_relay_observations(
    state: &EmporiumState,
    store: &RefCell<Store>,
    claude_home: &ClaudeHome,
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
    let roster_start = Instant::now();
    let brigades = match store.borrow().list_brigades() {
        Ok(brigades) => brigades,
        Err(_) => return Vec::new(),
    };
    if let Some(t) = timings.as_deref_mut() {
        t.relay_roster += roster_start.elapsed();
    }
    let live_scan_start = Instant::now();
    let live = read_live_sessions(&claude_home.sessions_dir());
    if let Some(t) = timings.as_deref_mut() {
        t.relay_live_scan += live_scan_start.elapsed();
    }
    // One shared process-table refresh for every Claude member this tick,
    // not one per member: `SysinfoProbe::is_alive` was costing a full
    // `CreateToolhelp32Snapshot`-and-walk-every-process pass PER CALL on
    // Windows (sysinfo 0.39.6 — `ProcessesToUpdate::Some` still enumerates
    // the whole table before filtering), so a brigade with several Claude
    // members paid that multiple times a second. `alive_claude` is a plain
    // local, scoped to this one call of this function and never stored
    // anywhere with a longer lifetime (not in `EmporiumState`, not in
    // `TickTimings`, not returned) — it cannot outlive this tick, and
    // `gather_relay_observations` runs fresh every `TICK_INTERVAL`, so a
    // member's liveness answer is never more than one tick stale, same as
    // before this change (this function was already only checked once a
    // tick; batching how the OS is asked doesn't change how often the
    // answer itself is asked for).
    let claude_snapshot_start = Instant::now();
    let alive_claude = session::alive_claude_pids(live.iter().map(|entry| entry.pid));
    if let Some(t) = timings.as_deref_mut() {
        t.relay_claude_snapshot += claude_snapshot_start.elapsed();
    }
    let mut observations = Vec::new();
    // One rowid lookup per tick: while no message has been appended, no
    // member's glance can have a new answer, so none is re-read.
    let glance_start = Instant::now();
    let newest_message = store.borrow().max_brigade_message_id().ok();
    if let Some(t) = timings.as_deref_mut() {
        t.relay_glance += glance_start.elapsed();
    }
    for brigade in brigades {
        let roster_start = Instant::now();
        let members = match store.borrow().brigade_members(brigade.id) {
            Ok(members) => members,
            Err(_) => continue,
        };
        if let Some(t) = timings.as_deref_mut() {
            t.relay_roster += roster_start.elapsed();
        }
        // What names this brigade to the operator, from the roster just read:
        // no further query. A brigade whose roster could not be read is not
        // listed (core keeps what it had). The members' display names are the
        // whole roster's — a Worker with no session id yet has no observation
        // below, but its tile is on screen and its Director may already have
        // named it.
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
        });
        for member in &members {
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
                    let idle = live
                        .iter()
                        .find(|entry| entry.session_id.as_deref() == Some(session_id.0.as_str()))
                        .map(|entry| {
                            alive_claude.contains(&entry.pid)
                                && !matches!(
                                    entry.status.as_deref(),
                                    Some(LIVE_STATUS_BUSY) | Some(LIVE_STATUS_WAITING)
                                )
                        });
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
                glance,
            });
        }
    }
    glance_gate.end_tick();
    observations
}

/// What the staged brigade's Goinkyo row (if any) means for
/// `engine::update_goinkyo_awaiting_spawn` this tick — see
/// [`GoinkyoObservation`]'s own doc for what each case tells the core to do.
/// A store read failure is reported as [`GoinkyoObservation::Unchanged`],
/// same as "no brigade staged": it is not itself evidence the row is gone,
/// and misreporting it as [`GoinkyoObservation::NoGoinkyo`] would release
/// the one-shot guard on a transient glitch rather than an actual ended
/// consultation.
///
/// Deliberately does *not* also check whether a pane is already open or in
/// flight for a would-be candidate: that would need the same synthetic key
/// `stage_brigade`'s own Worker-awaiting-discovery branch builds
/// (`SessionKey::new_worker`), which is private to `banto_core::engine` —
/// this crate has no way to construct one to look `state.screens`/
/// `pending_opens` up by. `update_goinkyo_awaiting_spawn` does that check
/// itself instead, on the core side where the constructor is reachable; see
/// its own doc for why `EmporiumState::goinkyo_pane` is the primary guard
/// either way, and that lookup only a defensive second one.
fn gather_goinkyo_observation(
    state: &EmporiumState,
    store: &RefCell<Store>,
    app: &App,
    mut timings: Option<&mut TickTimings>,
) -> GoinkyoObservation {
    // A consultation that ended, first and independently of the stage: the
    // Director's session can call `dismiss_goinkyo` while the operator is
    // looking anywhere at all, and the pane it leaves behind keeps running
    // (off-stage panes are pumped too). Asked only about brigades banto has
    // a Goinkyo pane recorded for, so this is one roster read for the one
    // entry that normally exists, not a scan of the store.
    for brigade_id in state.goinkyo_brigades() {
        let roster_start = Instant::now();
        let members = store.borrow().brigade_members(brigade_id);
        if let Some(t) = timings.as_deref_mut() {
            t.goinkyo_roster += roster_start.elapsed();
        }
        let Ok(members) = members else {
            continue;
        };
        if !members.iter().any(|m| m.role == BrigadeRole::Goinkyo) {
            return GoinkyoObservation::NoGoinkyo { brigade_id };
        }
    }
    let Stage::Brigade { id, director, .. } = &state.stage else {
        return GoinkyoObservation::Unchanged;
    };
    let brigade_id = *id;
    let roster_start = Instant::now();
    let members = store.borrow().brigade_members_in_join_order(brigade_id);
    if let Some(t) = timings {
        t.goinkyo_roster += roster_start.elapsed();
    }
    let Ok(members) = members else {
        return GoinkyoObservation::Unchanged;
    };
    let Some(goinkyo) = members.iter().find(|m| m.role == BrigadeRole::Goinkyo) else {
        return GoinkyoObservation::NoGoinkyo { brigade_id };
    };
    if goinkyo.session_id.is_some() {
        return GoinkyoObservation::Unchanged;
    }
    // Same cwd resolution as `add_worker`: the Director's own row — a
    // Goinkyo has no cwd of its own to fall back to, and belongs in the
    // same working directory the rest of the brigade already runs in.
    // `director: None` (unresolved, or its own pane closed) falls back to
    // "." the same as `add_worker`'s does.
    let director_row = director
        .as_ref()
        .and_then(|key| app.row_for_id(key.as_str()));
    let cwd = director_row
        .and_then(|row| row.cwd.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    GoinkyoObservation::AwaitingSpawn(GoinkyoSpawnCandidate {
        brigade_id,
        cwd,
        // Already in join order: what the core needs to place the
        // consultation among the Workers (see the field's doc).
        roster: members.iter().map(|m| m.token.clone()).collect(),
    })
}

/// How many parent-pid hops [`gather_fork_observations`] walks looking for a
/// staged pane's own child pid in a live entry's ancestry — covers `claude`
/// launched through a cmd/npm shim (a couple of hops) with headroom to
/// spare, without letting a pathological chain spin forever.
const FORK_ANCESTRY_DEPTH: u32 = 5;

/// Detect a live brigade member's Claude session having forked in place.
///
/// Claude Code's AUTO-compaction assigns a session a *new* id while leaving
/// the process itself running (manual `/compact` does not) — the same
/// `sessions/<pid>.json` live-state file the process has always published
/// simply starts reporting a different `sessionId`. One
/// `Event::MemberSessionForked` per staged Claude member whose recorded id
/// no longer matches what its own pane's process is actually reporting; the
/// pid match tries the pane's direct child first, falling back to an
/// ancestry walk for the cmd/npm-shim case (see [`FORK_ANCESTRY_DEPTH`]).
/// Repeats every tick until the store row (and the core's rename it
/// triggers) catch up — `engine::update_member_session_forked` is written
/// to tolerate that. A non-Claude member (resolved via `app`) never reaches
/// any of this — see the per-member gate's own comment for why the
/// mechanism cannot apply to one at all, not just why skipping it is fast.
fn gather_fork_observations(
    state: &EmporiumState,
    store: &RefCell<Store>,
    claude_home: &ClaudeHome,
    handles: &HashMap<SessionKey, PtyHandle>,
    app: &App,
    mut timings: Option<&mut TickTimings>,
) -> Vec<Event> {
    let roster_start = Instant::now();
    let brigades = match store.borrow().list_brigades() {
        Ok(brigades) => brigades,
        Err(_) => return Vec::new(),
    };
    if let Some(t) = timings.as_deref_mut() {
        t.fork_roster += roster_start.elapsed();
    }
    let live_scan_start = Instant::now();
    let live = read_live_sessions(&claude_home.sessions_dir());
    if let Some(t) = timings.as_deref_mut() {
        t.fork_live_scan += live_scan_start.elapsed();
    }
    let probe = SysinfoProbe;
    let mut events = Vec::new();
    let per_member_start = Instant::now();
    for brigade in brigades {
        let roster_start = Instant::now();
        let members = match store.borrow().brigade_members(brigade.id) {
            Ok(members) => members,
            Err(_) => continue,
        };
        if let Some(t) = timings.as_deref_mut() {
            t.fork_roster += roster_start.elapsed();
        }
        for member in &members {
            let Some(session_id) = member.session_id.as_ref() else {
                continue;
            };
            // Auto-compaction — the only thing this function ever detects — is
            // a Claude Code behavior: `sessions/<pid>.json` (what `live` reads)
            // is Claude's own live-state file, and a Codex member's session id
            // is never in it. Without this gate, that permanent absence read
            // every tick as "the id vanished," this function's own fork
            // signal, so a Codex member fell through to the ancestry walk
            // below on a mechanism that can never apply to it — not a slow
            // path for it, a meaningless one. Unresolved (`None`, discovery
            // hasn't caught up to this session yet) is treated the same as
            // Codex: skip rather than guess: the next tick's re-check catches
            // it once the product is known, the same tolerance this function's
            // own doc already claims for the fork signal itself.
            if app.row_for_id(&session_id.0).map(|row| row.agent) != Some(AgentKind::ClaudeCode) {
                continue;
            }
            let old_key = SessionKey::from_id(&session_id.0);
            if !state.screens.contains_key(&old_key) {
                continue;
            }
            let Some(pid) = handles.get(&old_key).and_then(PtyHandle::pid) else {
                continue;
            };
            // Steady-state short-circuit: as long as *some* live entry still
            // reports the recorded id, no fork has happened to this member — a
            // process can't report the old and the new id at once, and a fork
            // flips its one `sessions/<pid>.json` in place, so the recorded id
            // vanishing from the live set is a precondition of a fork. Checking
            // that first keeps the (sysinfo-backed) ancestry walks below off
            // the every-second tick path entirely until something actually
            // changed.
            if live
                .iter()
                .any(|entry| entry.session_id.as_deref() == Some(session_id.0.as_str()))
            {
                continue;
            }
            let forked = live.iter().find(|entry| {
                entry
                    .session_id
                    .as_deref()
                    .is_some_and(|id| !id.is_empty() && id != session_id.0)
                    && ancestry_reaches(entry.pid, pid, &probe, FORK_ANCESTRY_DEPTH)
            });
            if let Some(new_id) = forked.and_then(|entry| entry.session_id.clone()) {
                events.push(Event::MemberSessionForked {
                    brigade_id: brigade.id,
                    token: member.token.clone(),
                    old_id: session_id.0.clone(),
                    new_id,
                });
            }
        }
    }
    if let Some(t) = timings {
        t.fork_per_member += per_member_start.elapsed();
    }
    events
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
    let template = deps.brigade.prompt_for(role)?;
    let peers = crate::briefing::peers_of(&deps.store.borrow(), brigade_id, role);
    let request = (role == BrigadeRole::Goinkyo)
        .then(|| goinkyo_request_path(brigade_id))
        .flatten();
    Some(crate::briefing::render(
        template,
        brigade_id,
        token,
        &peers,
        request.as_deref(),
    ))
}

/// Where `crate::mcp::tool_consult_goinkyo` wrote this brigade's
/// consultation request. Recomputed here rather than threaded through
/// [`Deps`]: matches [`write_mcp_config`]'s own precedent of resolving
/// banto's data directory directly at the point of use rather than
/// injecting it. The join itself is `crate::mcp::resolve_goinkyo_dir` /
/// `crate::mcp::goinkyo_request_path` — the same two calls
/// `write_goinkyo_request` makes — rather than repeated here by hand: see
/// `resolve_goinkyo_dir`'s own doc for why a second hand-written copy would
/// be a bug waiting for only one side to be edited.
fn goinkyo_request_path(brigade_id: BrigadeId) -> Option<String> {
    let dir = crate::mcp::resolve_goinkyo_dir()?;
    Some(
        crate::mcp::goinkyo_request_path(&dir, brigade_id)
            .to_string_lossy()
            .into_owned(),
    )
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
fn brigade_env(brigade: Option<&(BrigadeId, MemberToken, BrigadeRole)>) -> Vec<(String, String)> {
    let Some((brigade_id, token, role)) = brigade else {
        return Vec::new();
    };
    vec![
        ("BANTO_BRIGADE".to_string(), brigade_id.to_string()),
        ("BANTO_MEMBER".to_string(), token.clone()),
        ("BANTO_ROLE".to_string(), role.as_token().to_string()),
    ]
}

/// Write a per-member `--mcp-config` file wiring the embedded claude to
/// banto's own MCP server (`banto _mcp`) with this member's brigade
/// identity, and return its path. Named by `(brigade_id, token)` rather than
/// the session id, since that's the only identity known upfront for a
/// freshly-spawned Worker. Lives under banto's own data dir, never under
/// `~/.claude`.
fn write_mcp_config(
    brigade_id: BrigadeId,
    token: &str,
    role: BrigadeRole,
    session_id: Option<&str>,
) -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let mut args = vec![
        "_mcp".to_string(),
        "--brigade".to_string(),
        brigade_id.to_string(),
        "--member".to_string(),
        token.to_string(),
        "--role".to_string(),
        role.as_token().to_string(),
    ];
    if let Some(session_id) = session_id {
        args.push("--session".to_string());
        args.push(session_id.to_string());
    }
    let config = serde_json::json!({
        "mcpServers": {
            "banto": {
                "command": exe.to_string_lossy(),
                "args": args,
            }
        }
    });
    let dir = dirs::data_local_dir()
        .map(|base| base.join("banto").join("mcp"))
        .ok_or_else(|| anyhow::anyhow!("could not determine banto's data directory"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{brigade_id}-{}.json", sanitize_filename(token)));
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
    Ok(path)
}

/// Keep a token safe as a filename component (tokens are `director`/`worker-N`,
/// but be defensive).
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
        format!("banto · waiting {} (Claude)", state.visible_waiting_count())
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
            .title(pane_title("session", false))
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
            let collapsed = geometry.collapsed[index];
            // A collapsed tile is never the focused one (the core keeps focus
            // off it), so its border carries attention only.
            let focused_tile =
                !collapsed && focus == Focus::Pane && focused_key.as_ref() == Some(key);
            let waiting = state.attention_panes().contains(key);
            let marked = marked_tile == Some(key);
            let block = tile_block(&state.tile_label(key), focused_tile, waiting, marked);
            let content = block.inner(*rect);
            frame.render_widget(block, *rect);
            paint_tile_buttons(
                frame.buffer_mut(),
                &geometry,
                key,
                tile_border_color(focused_tile, waiting, marked),
                state.soloed_key() == Some(key),
            );
            if collapsed {
                // The child keeps running untouched; only its facts are shown.
                let row = app.row_for_id(key.as_str());
                let facts = state.member_facts(key);
                let data = StripData {
                    // The session list's row when there is one; the product
                    // the relay resolved from the store before it has.
                    agent: row
                        .map(|row| row.agent)
                        .or_else(|| facts.and_then(|facts| facts.agent)),
                    activity: row.map(|row| row.activity),
                    output_age: state
                        .last_output_at(key)
                        .map(|at| tick.saturating_duration_since(at)),
                    inbound: facts
                        .and_then(|facts| facts.inbound.as_ref())
                        .map(strip_message),
                    sent: facts
                        .and_then(|facts| facts.sent.as_ref())
                        .map(strip_message),
                };
                render_strip(frame.buffer_mut(), content, &data, now);
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
        // `true`: the emporium binds Shift-Tab to `App::modal_toggle_agent`
        // (see `engine::update_modal_key`) — see `render_modal`'s doc.
        banto_tui::render_modal::render_modal(frame, modal, full_area, true);
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
/// count on the right.
fn render_status_bar(frame: &mut ratatui::Frame, app: &App, state: &EmporiumState, area: Rect) {
    let counts = format!("[{}/{}]", app.filtered_len(), app.total_len());
    let counts_width = counts.chars().count() as u16;
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(counts_width)]).areas(area);

    let (text, color) = if state.prefix_armed.is_some() {
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
    frame.render_widget(
        Paragraph::new(Span::styled(
            counts,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        right,
    );
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

/// A pane's title, padded so it sits in the border's own gap
/// (`┌ title ┐`) instead of colliding with it (`┌title┐`). The focused
/// pane's title is reversed video — `Modifier::REVERSED` swaps the pane's
/// own background in under the focus colour — so focus reads as a tab even
/// when the operator is looking at the middle of the screen, not the
/// border. An unfocused title names the terminal's default foreground
/// (`Color::Reset`) and is not dimmed: it is drawn over the border row, so a
/// title with no foreground of its own inherited the unfocused border's
/// `DarkGray`, and the dimming on top made it close to invisible on a dark
/// terminal theme (operator report, 2026-11-10). The default foreground is
/// readable on both dark and light themes, where any fixed grey is not. Named
/// colours only (see `border_style`, whose focus colour this reuses): the
/// reversal and the default resolve through the user's own terminal theme
/// rather than a hard-coded truecolor pair.
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
        Span::styled(padded, Style::default().fg(Color::Reset))
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
/// Unix backends behave correctly. The chōba list TUI (`crate::tui`) is
/// untouched: it has its own, separate `setup_terminal`.
fn setup_terminal() -> Result<(Tui, FrameOutputHandle<Stdout>)> {
    install_panic_hook();
    enable_raw_mode()?;
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
    execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange
    )?;
    Ok(())
}

/// Open the diagnostic input-event log when `BANTO_INPUT_LOG` is set —
/// mirrors `crate::tui`'s own instrumentation (same env var, same
/// `{ms} <prefix>: <message>` line format) line-for-line except for the
/// `emporium:` prefix in place of `tui:`, so a log the two modes happen to
/// share (they never run in the same process, but an operator may point
/// both at one file across separate runs) still says which mode wrote which
/// line. Diagnostic plumbing only: never read by anything in this crate,
/// and `engine.rs` (the pure core) never sees it.
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
/// `gather_fork_observations`/`gather_relay_observations`/
/// `gather_goinkyo_observation` as they run, so `tick`'s own cost in
/// [`StageTimings`] is attributed rather than guessed at. Same measurement
/// discipline as [`StageTimings`]: accumulation is unconditional and cheap
/// (`Instant::now()` deltas), only the eventual log write is gated. Per-member
/// work inside each function's own loop accumulates into these fields as it
/// goes rather than logging per member — one line at the end either way,
/// same as everything else this file measures.
#[derive(Default)]
struct TickTimings {
    fork_roster: Duration,
    fork_live_scan: Duration,
    fork_per_member: Duration,
    relay_roster: Duration,
    relay_live_scan: Duration,
    /// The one shared `SysinfoProbe::alive_pids` refresh every Claude
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
            "{ms} emporium: slow tick fork_roster={}us fork_live_scan={}us fork_per_member={}us \
             relay_roster={}us relay_live_scan={}us relay_claude_snapshot={}us relay_unseen={}us \
             relay_glance={}us relay_activity_claude={}us relay_activity_codex_sqlite={}us \
             relay_activity_codex_rollout={}us goinkyo_roster={}us",
            tick_timings.fork_roster.as_micros(),
            tick_timings.fork_live_scan.as_micros(),
            tick_timings.fork_per_member.as_micros(),
            tick_timings.relay_roster.as_micros(),
            tick_timings.relay_live_scan.as_micros(),
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

/// Compact, payload-free description of one raw crossterm event, logged
/// before conversion — deliberately a paste's length, never its text (the
/// diagnostic log is meant to be safe to paste into a bug report).
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
            let _ = execute!(
                io::stdout(),
                LeaveAlternateScreen,
                DisableMouseCapture,
                DisableBracketedPaste,
                DisableFocusChange
            );
            original(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use banto_core::model::{Activity, AgentKind, SessionRow};
    use banto_io::input::mock::MockInputReader;
    use banto_io::pty::mock::MockPtyHost;
    use banto_io::status::ProcessProbe;

    use ratatui::widgets::Widget as _;

    use super::*;

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
                result: Ok((1, vec!["worker-1".to_string(), "worker-2".to_string()])),
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
            Event::Spawned { key: director },
            now,
        );
        opened.extend(cmds.iter().filter_map(|cmd| match cmd {
            Cmd::OpenEmbedded { key, .. } => Some(key.clone()),
            _ => None,
        }));
        for key in opened.into_iter().skip(1) {
            engine::update(&mut state, &mut app, &brigade, Event::Spawned { key }, now);
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
        assert_eq!(pressed[(moved.x + 2, moved.y)].fg, DRAG_LINE_COLOR);
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

    #[test]
    fn a_collapsed_tile_shows_its_title_buttons_and_status_row_and_shows_waiting() {
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
        // Status row: agent, activity (waiting, in the waiting colour), and
        // the honest "no output yet" for a pane that has not written.
        let row = strip.y + 1;
        assert_eq!(
            text_at(&buffer, strip.x + 1, row, 32).trim_end(),
            "Claude \u{b7} waiting \u{b7} no output yet"
        );
        assert_eq!(buffer[(strip.x + 1 + 9, row)].fg, WAITING_ACTIVITY_COLOR);
        // Rows two and three are blank in part A.
        for y in [strip.y + 2, strip.y + 3] {
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
            })
        );
        assert_eq!(
            by_id(2),
            Some(BrigadeNameFacts {
                brigade_id: 2,
                name: "Second task".to_string(),
                director_session: None,
                member_names: Vec::new(),
            })
        );
        assert_eq!(by_id(3).map(|f| f.director_session), Some(None));
        assert_eq!(facts.len(), 3, "every visited brigade, once");
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
            glance,
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

        // The agent comes from the relay's facts: the list has no row for it.
        assert!(text_at(&buffer, strip.x + 1, strip.y + 1, 12).starts_with("Codex"));
        let inbound = text_at(&buffer, strip.x + 1, strip.y + 2, inner);
        assert!(
            inbound.starts_with("\u{2190} 5m run the whole suite and report"),
            "{inbound:?}"
        );
        let sent = text_at(&buffer, strip.x + 1, strip.y + 3, inner);
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
        assert_eq!(buffer[(right, strip.y + 3)].symbol(), "\u{2502}");

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
            text_at(&buffer, strip.x + 1, strip.y + 2, 20).starts_with("\u{2190} 5m run the whole")
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
                    Cmd::OpenEmbedded { key, .. } => {
                        queue.push_back(Event::Spawned { key: key.clone() })
                    }
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
    fn a_collapsed_tiles_output_age_comes_from_the_panes_last_output() {
        use banto_core::input::{MouseButton, MouseEventKind};
        let (mut state, mut app, brigade) = staged_brigade_of_two_workers();
        let area = Rect::new(0, 0, 120, 40);
        let key = SessionKey::from_id("new-worker::1::worker-1");
        engine::update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key: key.clone(),
                chunk: b"x".to_vec(),
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

        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        let mut cache = HashMap::new();
        let mut row_at = |tick: Instant| {
            terminal
                .draw(|frame| {
                    let _ = draw(frame, &app, &state, SystemTime::now(), tick, &mut cache);
                })
                .unwrap();
            text_at(terminal.backend().buffer(), strip.x + 1, strip.y + 1, 40)
        };
        let just_now = Instant::now();
        // The agent and activity are unknown for a pane the list has no row
        // for; the output age is still there.
        assert!(row_at(just_now).trim_end().ends_with("output now"));
        assert!(
            row_at(just_now + Duration::from_secs(25))
                .trim_end()
                .ends_with("output 20s ago")
        );
        assert!(
            row_at(just_now + Duration::from_secs(130))
                .trim_end()
                .ends_with("output 2m ago")
        );
        assert!(
            row_at(just_now + Duration::from_secs(7300))
                .trim_end()
                .ends_with("output 2h ago")
        );
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

    // --- brigade member auto-heal: resolve_membership follows lineage ----

    #[test]
    fn resolve_membership_heals_a_members_session_id_to_its_lineage_leaf() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(
                brigade_id,
                "worker-1",
                BrigadeRole::Worker,
                Some(&SessionId("w1-old".to_string())),
            )
            .unwrap();
        store
            .record_lineage(
                &SessionId("w1-new".to_string()),
                &SessionId("w1-old".to_string()),
            )
            .unwrap();
        let store = RefCell::new(store);

        // Resolve membership from the OLD id — the exact situation the live
        // watcher misses: the fork happened while banto wasn't watching, so
        // nothing ever rekeyed the pane, and the member row still records
        // the ancestor.
        let events = execute_store_intent(
            StoreIntent::ResolveMembership {
                session_id: "w1-old".to_string(),
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
        assert_eq!(
            worker.2.as_deref(),
            Some("w1-new"),
            "the roster handed to the engine must carry the healed id"
        );

        // Persisted, not just reported this once — a later re-stage must
        // see it too.
        assert_eq!(
            store
                .borrow()
                .brigade_member(brigade_id, "worker-1")
                .unwrap()
                .unwrap()
                .session_id,
            Some(SessionId("w1-new".to_string()))
        );
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
        let superseded_failed = RefCell::new(HashSet::new());
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
            superseded_failed: &superseded_failed,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade: &brigade,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
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
        let superseded_failed = RefCell::new(HashSet::new());
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
            superseded_failed: &superseded_failed,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade: &brigade,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
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
        let superseded_failed = RefCell::new(HashSet::new());
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
            superseded_failed: &superseded_failed,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade: &brigade,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
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
        let superseded_failed = RefCell::new(HashSet::new());
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
            superseded_failed: &superseded_failed,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade: &brigade,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
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
            cwd: cwd.clone(),
            since: SystemTime::now(),
            member: Some((1, "worker-1".to_string())),
            pid: Some(4242),
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
            cwd: PathBuf::from("/work/alpha"),
            since: SystemTime::now(),
            member: Some((1, "worker-1".to_string())),
            pid: Some(4242),
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
            cwd: cwd.clone(),
            since,
            member: Some((1, token.to_string())),
            pid: None,
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

    // --- Codex Worker discovery: the store-based source, and giving up -----

    fn codex_tracker(brigade_id: BrigadeId, token: &str, since: SystemTime) -> DiscoveryTracker {
        DiscoveryTracker {
            key: pending_key(token),
            agent: AgentKind::Codex,
            cwd: PathBuf::from("/work/alpha"),
            since,
            member: Some((brigade_id, token.to_string())),
            pid: None,
            notified_untrusted: false,
        }
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

        let mut trackers = vec![codex_tracker(brigade_id, "worker-1", SystemTime::now())];

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
            cwd: PathBuf::from("/work/alpha"),
            since: SystemTime::now(),
            member: Some((brigade_id, "worker-1".to_string())),
            pid: None,
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
    fn a_codex_worker_past_the_timeout_gives_up_and_is_removed() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let claude_home = tempfile::tempdir().unwrap();
        let provider = ClaudeCodeProvider::new(ClaudeHome::new(claude_home.path().to_path_buf()));
        let claude_home_ref = ClaudeHome::new(claude_home.path().to_path_buf());
        let long_ago = SystemTime::now() - CODEX_WORKER_DISCOVERY_TIMEOUT - Duration::from_secs(1);
        let mut trackers = vec![codex_tracker(1, "worker-1", long_ago)];

        let events = poll_discovery(
            &mut trackers,
            &provider,
            &HashSet::new(),
            &[],
            &store,
            &claude_home_ref,
        );

        assert!(matches!(
            events.as_slice(),
            [Event::CodexWorkerDiscoveryTimedOut { token, .. }] if token == "worker-1"
        ));
        assert!(trackers.is_empty());
    }

    #[test]
    fn a_claude_worker_never_times_out() {
        // Regression: the give-up timeout is Codex-only. Unresolved Claude
        // discovery keeps waiting forever, same as before this existed.
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let claude_home = tempfile::tempdir().unwrap();
        let provider = ClaudeCodeProvider::new(ClaudeHome::new(claude_home.path().to_path_buf()));
        let claude_home_ref = ClaudeHome::new(claude_home.path().to_path_buf());
        let long_ago = SystemTime::now() - CODEX_WORKER_DISCOVERY_TIMEOUT - Duration::from_secs(1);
        let mut trackers = vec![DiscoveryTracker {
            key: pending_key("worker-1"),
            agent: AgentKind::ClaudeCode,
            cwd: PathBuf::from("/work/alpha"),
            since: long_ago,
            member: Some((1, "worker-1".to_string())),
            pid: None,
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
        assert_eq!(trackers.len(), 1, "still pending, not timed out");
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
            cwd: PathBuf::from("/work/alpha"),
            since: SystemTime::now(),
            member: Some((1, "worker-1".to_string())),
            pid: None,
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
            cwd: PathBuf::from("/work/alpha"),
            since: SystemTime::now(),
            member: Some((1, "worker-1".to_string())),
            pid: None,
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
        // Defends the `agent == AgentKind::ClaudeCode` gate itself: a Codex
        // tracker has its own, unrelated trust gate
        // (`execute_check_worker_directory_trust`, core-side, per-Worker
        // kickoff); this notice exists only to explain Claude's silence.
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
            "still pending — Codex has its own gate, not this one"
        );
    }

    // --- compact-fork tracking: gather_fork_observations ------------------

    #[test]
    fn gather_fork_observations_detects_a_forked_worker_by_exact_pid_match() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1-old", Some(AgentKind::ClaudeCode));
        write_live_state(claude_home.path(), 4242, "w1-new", Path::new("/work/alpha"));

        let mut handles = HashMap::new();
        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        handles.insert(SessionKey::from_id("w1-old"), open(&host));

        let events = gather_fork_observations(
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
            "expected exactly one fork observation: {events:?}"
        );
    }

    #[test]
    fn gather_fork_observations_detects_an_offstage_members_compaction_fork() {
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

        let events = gather_fork_observations(
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
            }] if *brigade_id == offstage_brigade
                && token == "worker-1"
                && old_id == "w2-old"
                && new_id == "w2-new"
        ));
    }

    #[test]
    fn gather_fork_observations_is_silent_when_the_live_entry_still_reports_the_recorded_id() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1", Some(AgentKind::ClaudeCode));
        write_live_state(claude_home.path(), 4242, "w1", Path::new("/work/alpha"));

        let mut handles = HashMap::new();
        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        handles.insert(SessionKey::from_id("w1"), open(&host));

        let events = gather_fork_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            &handles,
            &app,
            None,
        );

        assert!(events.is_empty(), "no fork happened: {events:?}");
    }

    #[test]
    fn gather_fork_observations_never_enters_the_ancestry_path_for_a_codex_member() {
        // Same exact-pid-match setup as
        // `gather_fork_observations_detects_a_forked_worker_by_exact_pid_match`
        // — `entry.pid == pid` would trip `ancestry_reaches` at depth 0
        // with no real OS lookup at all, so if the Claude-only gate above
        // this test's own body ever regressed, this would reliably produce
        // a (wrong) fork observation for a Codex member instead of quietly
        // doing nothing. The only difference from the Claude test is the
        // agent tag `staged_worker` puts in `App`'s row.
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1-old", Some(AgentKind::Codex));
        write_live_state(claude_home.path(), 4242, "w1-new", Path::new("/work/alpha"));

        let mut handles = HashMap::new();
        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        handles.insert(SessionKey::from_id("w1-old"), open(&host));

        let events = gather_fork_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            &handles,
            &app,
            None,
        );

        assert!(
            events.is_empty(),
            "a Codex member must never produce a fork observation: {events:?}"
        );
    }

    #[test]
    fn gather_fork_observations_ignores_a_pane_with_no_known_child_pid() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state, app) = staged_worker("w1-old", Some(AgentKind::ClaudeCode));
        write_live_state(claude_home.path(), 4242, "w1-new", Path::new("/work/alpha"));

        // No handle registered at all for the pane's key, so its pid can
        // never be known — this must not panic, just find nothing.
        let handles = HashMap::new();

        let events = gather_fork_observations(
            &state,
            &store,
            &ClaudeHome::new(claude_home.path().to_path_buf()),
            &handles,
            &app,
            None,
        );

        assert!(events.is_empty());
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
    fn unfocused_pane_title_names_the_terminals_default_foreground_and_is_not_dimmed() {
        assert_eq!(
            pane_title("director", false).style,
            Style::default().fg(Color::Reset)
        );
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
        assert_eq!(cell.fg, Color::Reset);
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

    // --- worker model on resume: build_open_launch -------------------------

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

    /// An [`opener::OpenContext`] for tests that don't care about Codex
    /// liveness (`codex_home: None` degrades every Codex check to "not
    /// live" — see `codex_liveness::is_thread_alive`'s doc).
    fn test_ctx<'a>(
        probe: &'a dyn ProcessProbe,
        live: &'a [LiveSession],
        binaries: &'a AgentBinaries,
    ) -> opener::OpenContext<'a> {
        opener::OpenContext {
            probe,
            live,
            binaries,
            codex_home: None,
            start_time: &SysinfoStartTime,
        }
    }

    #[test]
    fn build_open_launch_appends_model_to_a_resumed_sessions_argv() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let binaries = AgentBinaries::default();
        let launch = build_open_launch(
            &open_target("sess-1"),
            Some("opus"),
            None,
            None,
            None,
            None,
            &test_ctx(&probe, &[], &binaries),
        )
        .unwrap();
        assert_eq!(
            launch.argv("claude"),
            ["claude", "--resume", "sess-1", "--model", "opus"].map(str::to_string)
        );
    }

    #[test]
    fn build_open_launch_appends_effort_right_after_model_and_omits_it_when_none() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let binaries = AgentBinaries::default();
        let launch = build_open_launch(
            &open_target(""),
            Some("fable"),
            Some("max"),
            None,
            None,
            None,
            &test_ctx(&probe, &[], &binaries),
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
            None,
            None,
            None,
            &test_ctx(&probe, &[], &binaries),
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
        let binaries = AgentBinaries::default();
        let launch = build_open_launch(
            &open_target(""),
            Some("fable"),
            Some("max"),
            Some("auto"),
            None,
            None,
            &test_ctx(&probe, &[], &binaries),
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
            None,
            None,
            None,
            &test_ctx(&probe, &[], &binaries),
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
        let binaries = AgentBinaries::default();
        let launch = build_open_launch(
            &open_target(""),
            Some("fable"),
            Some("max"),
            Some("auto"),
            Some("Edit,Write,NotebookEdit"),
            None,
            &test_ctx(&probe, &[], &binaries),
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
            Some("auto"),
            None,
            None,
            &test_ctx(&probe, &[], &binaries),
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
        let binaries = AgentBinaries::default();
        let launch = build_open_launch(
            &open_target(""),
            Some("opus"),
            None,
            None,
            None,
            None,
            &test_ctx(&probe, &[], &binaries),
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
        let binaries = AgentBinaries::default();
        let launch = build_open_launch(
            &open_target("sess-1"),
            None,
            None,
            None,
            None,
            None,
            &test_ctx(&probe, &[], &binaries),
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
            kind: None,
            name: None,
            proc_start: None,
            version: None,
        }];
        let binaries = AgentBinaries::default();
        assert!(
            build_open_launch(
                &open_target("sess-1"),
                Some("opus"),
                None,
                None,
                None,
                None,
                &test_ctx(&probe, &live, &binaries),
            )
            .is_none()
        );
    }

    #[test]
    fn build_open_launch_codex_target_resumes_via_codex_with_cwd() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let mut target = open_target("codex-uuid-1");
        target.agent = AgentKind::Codex;
        let binaries = AgentBinaries::default();
        let launch = build_open_launch(
            &target,
            Some("o3"),
            // `effort`/`permission_mode`/`disallowed_tools` are all
            // Claude-only (`AgentLaunch::Claude`'s own doc); dropped here
            // for the same reason the briefing below is.
            None,
            None,
            None,
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
            &test_ctx(&probe, &[], &binaries),
        )
        .unwrap();
        assert_eq!(
            launch.argv("codex"),
            [
                "codex",
                "resume",
                "codex-uuid-1",
                "-m",
                "o3",
                "-c",
                "tui.notifications=[\"approval-requested\"]",
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
        let binaries = AgentBinaries::default();
        let launch = build_open_launch(
            &target,
            Some("o3"),
            None,
            Some("auto"),
            None,
            None,
            &test_ctx(&probe, &[], &binaries),
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
    fn build_open_launch_appends_the_briefing_after_the_model() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let binaries = AgentBinaries::default();
        let launch = build_open_launch(
            &open_target("sess-1"),
            Some("opus"),
            None,
            None,
            None,
            Some("you are the Director"),
            &test_ctx(&probe, &[], &binaries),
        )
        .unwrap();
        assert_eq!(
            launch.argv("claude"),
            [
                "claude",
                "--resume",
                "sess-1",
                "--model",
                "opus",
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
        let binaries = AgentBinaries::default();
        let launch = build_open_launch(
            &open_target("sess-1"),
            None,
            None,
            None,
            None,
            None,
            &test_ctx(&probe, &[], &binaries),
        )
        .unwrap();
        match launch {
            opener::AgentLaunch::Claude { mcp_config, .. } => assert_eq!(mcp_config, None),
            opener::AgentLaunch::Codex { .. } => panic!("expected a Claude launch"),
        }
    }

    #[test]
    fn brigade_env_carries_the_identity_the_hook_command_must_not() {
        assert_eq!(
            brigade_env(Some(&(7, "worker-1".to_string(), BrigadeRole::Worker))),
            vec![
                ("BANTO_BRIGADE".to_string(), "7".to_string()),
                ("BANTO_MEMBER".to_string(), "worker-1".to_string()),
                ("BANTO_ROLE".to_string(), "worker".to_string()),
            ]
        );
    }

    #[test]
    fn brigade_env_is_empty_outside_a_brigade() {
        assert!(brigade_env(None).is_empty());
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
        let superseded_failed = RefCell::new(HashSet::new());
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
            superseded_failed: &superseded_failed,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
        };

        assert_eq!(
            member_briefing(
                &deps(&config),
                brigade_id,
                "director",
                BrigadeRole::Director
            ),
            Some("I am director; my team is worker-1, worker-2".to_string())
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
        let superseded_failed = RefCell::new(HashSet::new());
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
            superseded_failed: &superseded_failed,
            meta_cache: &meta_cache,
            pty_host: &pty_host,
            input_reader: &input_reader,
            brigade: &config,
            agent_binaries: &agent_binaries,
            enabled_agents: &enabled_agents,
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
    fn goinkyo_request_path_matches_what_the_shared_join_computes() {
        // `expected` is computed by calling `mcp::resolve_goinkyo_dir` /
        // `mcp::goinkyo_request_path` directly here, independently of
        // whatever this function's own body currently does — not just
        // pinning the `.../goinkyo/42.txt` suffix as a hand-written
        // literal, which a differently-based independent join could still
        // satisfy. If this function's body ever stopped delegating to those
        // same two calls (reverting to its own hand-rolled join, even one
        // that happens to match today), or if `resolve_goinkyo_dir`'s own
        // definition changed without this function picking it up, the two
        // sides would diverge and this would fail.
        let expected = crate::mcp::resolve_goinkyo_dir().map(|dir| {
            crate::mcp::goinkyo_request_path(&dir, 42)
                .to_string_lossy()
                .into_owned()
        });
        assert_eq!(
            goinkyo_request_path(42),
            expected,
            "this platform is assumed to have a data-local dir for the test to be meaningful"
        );
    }

    // --- gather_goinkyo_observation ------------------------------------------

    /// A staged brigade of a Director plus a Goinkyo member row with no
    /// session id yet — the shape `gather_goinkyo_observation` reads.
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
        let mut dir_row = test_row("dir", AgentKind::ClaudeCode);
        dir_row.cwd = Some(PathBuf::from("/work/alpha"));
        let app = App::new(vec![dir_row]);
        (RefCell::new(store), state, app)
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

        let GoinkyoObservation::AwaitingSpawn(candidate) =
            gather_goinkyo_observation(&state, &store, &app, None)
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
        let observation = gather_goinkyo_observation(&state, &store, &app, None);
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
                gather_goinkyo_observation(&state, &store, &app, None),
                GoinkyoObservation::NoGoinkyo { brigade_id: id } if id == brigade_id
            ),
            "the ended consultation must be reported whatever is on stage"
        );
    }

    #[test]
    fn a_brigade_with_no_goinkyo_pane_recorded_is_not_asked_about() {
        // The scan above is over panes banto actually opened, not over the
        // store: a brigade that never had a consultation must not produce an
        // observation just because it has no Goinkyo row.
        let (store, mut state, app) = staged_goinkyo(None);
        let brigade_id = store.borrow().list_brigades().unwrap()[0].id;
        store
            .borrow_mut()
            .dismiss_worker(brigade_id, "goinkyo")
            .unwrap();
        state.stage = Stage::Empty;

        assert!(matches!(
            gather_goinkyo_observation(&state, &store, &app, None),
            GoinkyoObservation::Unchanged
        ));
    }

    #[test]
    fn a_goinkyo_row_with_no_session_id_is_a_spawn_candidate_in_the_directors_cwd() {
        let (store, state, app) = staged_goinkyo(None);
        let GoinkyoObservation::AwaitingSpawn(candidate) =
            gather_goinkyo_observation(&state, &store, &app, None)
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

        let GoinkyoObservation::AwaitingSpawn(candidate) =
            gather_goinkyo_observation(&state, &store, &app, None)
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
            gather_goinkyo_observation(&state, &store, &app, None),
            GoinkyoObservation::Unchanged
        );
    }

    #[test]
    fn no_goinkyo_member_at_all_reports_the_row_as_gone() {
        // Distinct from every other "nothing to report" case: this is what
        // tells `update_goinkyo_awaiting_spawn` a consultation was
        // dismissed out from under a still-staged brigade, so it can
        // release `goinkyo_pane` for a later one. Disband does not reach
        // this case at all — it un-stages the brigade first, so
        // the next observation is `Unchanged`, not this.
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
        let app = App::new(vec![test_row("dir", AgentKind::ClaudeCode)]);

        assert_eq!(
            gather_goinkyo_observation(&state, &RefCell::new(store), &app, None),
            GoinkyoObservation::NoGoinkyo { brigade_id }
        );
    }

    #[test]
    fn outside_a_staged_brigade_is_always_unchanged() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let state = EmporiumState::new(PrefixKey::default());
        let app = App::new(vec![]);
        assert_eq!(
            gather_goinkyo_observation(&state, &store, &app, None),
            GoinkyoObservation::Unchanged
        );
    }

    #[test]
    fn a_stranded_goinkyos_session_clears_for_real_and_respawns_exactly_once() {
        // The other half of the `stage_brigade` fix for a stranded Goinkyo,
        // exercised through the real store round trip `engine.rs`'s own
        // pure unit tests can't reach: does clearing `session_id` actually
        // flip `gather_goinkyo_observation` back to `AwaitingSpawn`, and
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
        let observation = gather_goinkyo_observation(&state, &store, &app, None);
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
        assert!(
            matches!(cmds.as_slice(), [Cmd::OpenEmbedded { .. }]),
            "expected exactly one respawn: {cmds:?}"
        );

        // A further tick before the respawned pane even reports back
        // (`Event::Spawned`, which is what actually records a session id
        // and would make `gather_goinkyo_observation` itself report
        // `Unchanged`) must not spawn a second one. The store still shows
        // `session_id: None` at this point — the shell's own observation is
        // correctly still `AwaitingSpawn`, same as just above; it's
        // `update_goinkyo_awaiting_spawn`'s own one-shot guard
        // (`EmporiumState::goinkyo_pane`, armed by the first spawn above)
        // that has to be the thing stopping a second one — the guard this
        // whole fix has to respect, not bypass.
        let observation_again = gather_goinkyo_observation(&state, &store, &app, None);
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
            cmds_again.is_empty(),
            "the one-shot spawn guard must stop a second respawn: {cmds_again:?}"
        );
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
        superseded_failed: RefCell<HashSet<SessionId>>,
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
                superseded_failed: RefCell::new(HashSet::new()),
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
                superseded_failed: &self.superseded_failed,
                meta_cache: &self.meta_cache,
                pty_host: &self.pty_host,
                input_reader: &self.input_reader,
                brigade: &self.brigade,
                agent_binaries: &self.agent_binaries,
                enabled_agents: &self.enabled_agents,
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
        }
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
                key: SessionKey::from_id("pane")
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
}
