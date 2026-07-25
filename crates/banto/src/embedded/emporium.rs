//! The "emporium" (大店 / `--emporium` / `--oodana`) mode: banto as a
//! persistent left sidebar (the session list) plus a right pane hosting the
//! selected session embedded.
//!
//! Since Phase 2a of the sans-IO migration (`docs/DISCIPLINE.md` §4), this
//! module is a thin **shell**: it gathers facts about the outside world into
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
//! `BANTO_RECORD_EVENTS=<path>` (see [`EventRecorder`]) captures every
//! `Event` fed into [`engine::update`] as a `docs/DISCIPLINE.md` §8 replay
//! stream — **a captured file is a LOCAL DIAGNOSTIC ARTIFACT and must never
//! be committed**: unlike `BANTO_INPUT_LOG`, it contains real session
//! content in full (keystrokes, pasted text, PTY output), not redacted
//! lengths. Repo invariant 2 applies with full force — `banto_core::replay`'s
//! own fixtures are hand-written synthetic streams only.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
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

use banto_core::app::{App, Mode};
use banto_core::config::{BrigadeConfig, KeysConfig};
use banto_core::engine::{
    self, Cmd, EmporiumState, Event, Focus, GroupJoinTargetData, PrefixKey, RelayObservation,
    SessionKey, Stage, StoreIntent, layout, stage_tiles,
};
use banto_core::input::InputEvent;
use banto_core::model::{BrigadeId, BrigadeRole, MemberToken, SessionId, SessionToOpen};
use banto_core::replay::{STREAM_VERSION, TimedEvent};
use banto_core::status::AgeThresholds;
use banto_io::provider::claude_code::ClaudeCodeProvider;
use banto_io::pty::PortablePtyHost;
use banto_io::status::{
    LiveSession, ProcessProbe, SysinfoProbe, ancestry_reaches, read_live_sessions,
};
use banto_io::store::Store;
use banto_tui::render::screen_to_text;
use banto_tui::view;

use crate::opener;
use crate::session;
use crate::tui::LiveWatch;

use super::convert;
use super::paste_accum::{PasteAccumulator, is_in_scope};
use super::session::{PtyHandle, PtyPoll, wait_for_exit_or_deadline};

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Run the emporium mode until the user quits (`q`/Esc from the sidebar).
/// `brigade` is `[brigade]` from config.toml: how many fresh Workers `B`
/// auto-spawns when forming a new brigade, the `--model` an auto-spawned
/// Worker launches with, and whether the relay engine is enabled. `keys` is
/// `[keys]`: the tmux-style prefix chord for pane operations.
pub fn run(
    claude_home: &Path,
    thresholds: &AgeThresholds,
    store: &RefCell<Store>,
    brigade: &BrigadeConfig,
    keys: &KeysConfig,
) -> Result<()> {
    // Janitor: purge brigades with no members left (legacy pre-v7 data, or
    // residue from a crash mid-formation) before the sidebar's brigade-
    // derived caches (hidden Workers, Directors) load. Silent by design — an
    // empty brigade is never user-visible, so there's nothing to report.
    let _ = store.borrow_mut().delete_empty_brigades();

    let rows = session::load_rows(claude_home, thresholds)?;
    // In-memory only, for this process's lifetime — see
    // `crate::tui::load_superseded`'s doc. Created once here and threaded
    // through every reload (the bootstrap below and every later
    // `gather_reload`) rather than per-call, so a permanently-unresolvable
    // continuation is scanned at most once per banto run.
    let superseded_failed = RefCell::new(HashSet::new());
    // Same store-backed state the chōba list builds, so grouping / pins /
    // archived-hiding / brigade hiding show identically in the sidebar. This
    // one-time bootstrap stays outside `update`: `App::with_*` are
    // construction-only builders, not a repeating decision.
    let (rows, pinned, groups, session_groups, hidden, directors, superseded) = {
        let store = store.borrow();
        let rows = crate::tui::exclude_archived(rows, &store);
        let pinned = crate::tui::load_pinned(&store);
        let groups = crate::tui::load_groups(&store);
        let session_groups = crate::tui::load_session_groups(&store, &groups);
        let hidden = crate::tui::load_hidden_worker_ids(&store);
        let directors = crate::tui::load_directors(&store);
        let superseded = crate::tui::load_superseded(claude_home, &store, &superseded_failed);
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
        .with_superseded(superseded);

    let deps = Deps {
        claude_home,
        thresholds,
        store,
        superseded_failed: &superseded_failed,
        brigade,
    };
    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, &mut app, &deps, keys);
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
    claude_home: &'a Path,
    thresholds: &'a AgeThresholds,
    store: &'a RefCell<Store>,
    /// See [`crate::tui::load_superseded`]'s doc: in-memory only, for this
    /// process's lifetime.
    superseded_failed: &'a RefCell<HashSet<SessionId>>,
    /// `[brigade]` from config.toml. Lives here rather than as its own
    /// `event_loop` parameter because `execute_cmd` needs it too now (a
    /// member's launch argv carries its role briefing — see
    /// [`render_briefing`]).
    brigade: &'a BrigadeConfig,
}

fn event_loop(terminal: &mut Tui, app: &mut App, deps: &Deps, keys: &KeysConfig) -> Result<()> {
    let brigade = deps.brigade;
    let mut state = EmporiumState::new(PrefixKey::parse(&keys.prefix));
    let mut handles: HashMap<SessionKey, PtyHandle> = HashMap::new();
    let mut discovery: Vec<DiscoveryTracker> = Vec::new();
    let mut watch = LiveWatch::new(deps.claude_home);
    let provider = ClaudeCodeProvider::new(deps.claude_home.to_path_buf());
    let mut last_tick: Option<Instant> = None;
    let mut input_log = open_input_log();
    let mut paste_acc = PasteAccumulator::new();
    let run_start = Instant::now();
    let mut event_recorder = open_event_recorder(run_start);

    loop {
        let now = Instant::now();
        let mut events: VecDeque<Event> = VecDeque::new();

        // Terminal geometry: only fed when it actually changed (`update`'s
        // handling is idempotent either way, but there's no reason to spam
        // it every ~50ms).
        let size = terminal.size()?;
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
        // handle reports it once, right here, then gets dropped below.
        for (key, handle) in &mut handles {
            loop {
                match handle.poll() {
                    PtyPoll::Chunk(chunk) => events.push_back(Event::PtyOutput {
                        key: key.clone(),
                        chunk,
                    }),
                    PtyPoll::Empty => break,
                    PtyPoll::Disconnected => {
                        events.push_back(Event::PtyExited { key: key.clone() });
                        break;
                    }
                }
            }
        }

        // One real input event, non-blocking with a short poll window (the
        // pacing knob for the whole loop, matching the pre-migration cadence)
        // — dropped from 50ms to 10ms while `paste_acc` holds buffered keys,
        // so a lone buffered key still flushes within one `PASTE_GAP` of
        // arriving instead of waiting on the ordinary idle cadence (see
        // `paste_accum`'s module doc).
        // Converted from crossterm at this boundary (`convert::from_crossterm`)
        // — `None` for an event kind banto ignores (a key release, focus
        // change, ...), which simply contributes nothing to this tick.
        let poll_timeout = if paste_acc.is_pending() {
            Duration::from_millis(10)
        } else {
            Duration::from_millis(50)
        };
        if event::poll(poll_timeout)? {
            // Timestamped separately from the loop's own `now` (above):
            // `paste_acc`'s gap timing must track real inter-keystroke
            // spacing, not spacing inflated by whatever PTY-pump/discovery
            // work this iteration happened to do before reaching the poll.
            let event_now = Instant::now();
            let raw = event::read()?;
            log_input(&mut input_log, &describe_raw_event(&raw));
            if let Some(input) = convert::from_crossterm(raw) {
                log_input(&mut input_log, &describe_converted_event(&input));
                if is_in_scope(&state, &input) {
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
        } else if paste_acc.is_pending()
            && let Some(flushed) = paste_acc.tick(Instant::now())
        {
            emit_flushed(&mut events, &mut input_log, flushed);
        }

        // Discovery: poll for the ids Claude assigns to freshly-launched
        // sessions still awaiting one. `claimed` — the ids already spoken
        // for — is read off the handle map, which is only truthful because
        // `Cmd::RekeyPty` renames a handle the moment its id is discovered:
        // without that, a resolved id stays invisible here and the next
        // pending tracker resolves onto it a second time.
        if !discovery.is_empty() {
            let claimed: HashSet<String> =
                handles.keys().map(|key| key.as_str().to_string()).collect();
            let live = read_live_sessions(&deps.claude_home.join("sessions"));
            events.extend(poll_discovery(&mut discovery, &provider, &claimed, &live));
        }

        // Live updates: reload the list once the watched dirs settle.
        if watch.poll_ready(SystemTime::now()) {
            events.extend(gather_reload(deps));
        }

        // ~1s: relay observations for the staged brigade, gathered here
        // (store + live-session reads) and decided in `update` — plus the
        // trigger for the pending-submit flush and status expiry bundled
        // into the same tick (see `engine::update_tick`'s doc).
        if last_tick.is_none_or(|tick| now.duration_since(tick) >= TICK_INTERVAL) {
            last_tick = Some(now);
            events.extend(gather_fork_observations(
                &state,
                deps.store,
                deps.claude_home,
                &handles,
            ));
            let relay = gather_relay_observations(&state, deps.store, deps.claude_home);
            events.push_back(Event::Tick { relay });
        }

        // Drain the event queue through `update`, executing the `Cmd`s it
        // returns and feeding any follow-up fact back onto the same queue —
        // the synchronous relaxation (DISCIPLINE §6.1) generalized from just
        // the store to every shell-executed `Cmd` (spawning a PTY child is
        // just as synchronous a call as a store write).
        while let Some(ev) = events.pop_front() {
            if let Some(recorder) = &mut event_recorder {
                recorder.record(&ev, now);
            }
            let cmds = engine::update(&mut state, app, brigade, ev, now);
            for cmd in cmds {
                events.extend(execute_cmd(cmd, deps, &mut handles, &mut discovery));
            }
        }

        // A `PtyExited` handler drops the session's `Screen`; the handle
        // itself (now pointing at a dead reader thread) is dropped here.
        // This is also why every core-side `screens` rekey must reach the
        // handle map (`Cmd::RekeyPty`): a handle left under a stale key
        // reads as "screen gone" and is reaped here, which on Unix closes
        // the PTY master and SIGHUPs a perfectly live child.
        handles.retain(|key, _| state.screens.contains_key(key));

        terminal.draw(|frame| draw(frame, app, &state, SystemTime::now()))?;

        if app.should_quit() {
            break;
        }
    }
    shutdown_handles(&mut handles, SHUTDOWN_GRACE, SHUTDOWN_POLL_INTERVAL);
    Ok(())
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
/// risks at most one truncated trailing line — an already-accepted
/// (Phase 2b) trade for responsiveness that this sweep does not need to
/// make, since nothing here is time-sensitive to the user.
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
/// that writes to a hosted session's stdin, spawns a process, or touches the
/// store.
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
            }
            Vec::new()
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
        Cmd::OpenEmbedded {
            key,
            target,
            brigade,
            model,
        } => execute_open_embedded(key, target, brigade, model, deps, handles, discovery),
        Cmd::Store(intent) => execute_store_intent(intent, deps.store),
        Cmd::Reload => gather_reload(deps),
    }
}

/// Build the base argv for opening `target`: resuming it via
/// [`opener::decide_inplace_resume`] when a real id is already known (`None`
/// if a live pane elsewhere refuses the resume), or a fresh unresumed launch
/// otherwise — either way with `--model` appended when `model` is `Some`.
/// `--model` applies the same way to a resume as to a fresh spawn: a Worker
/// resumed without it falls back to the operator's own default model
/// instead of the brigade's configured one (`engine::stage_brigade` never
/// sets it for the Director, so this is never reached for one).
///
/// `briefing` is the member's already-rendered role briefing
/// (`--append-system-prompt`, see [`render_briefing`]) — rendered by the
/// caller because it needs a store read for the roster, verified against
/// `claude` 2.1.219 to apply to a `--resume` exactly as it does to a fresh
/// launch, which is what makes it reach a resumed Director at all.
///
/// Pulled out of [`execute_open_embedded`] so this branch-and-append logic
/// is unit-testable without spawning a real PTY; the `--mcp-config` append
/// stays in the caller, since that needs real file I/O this doesn't.
fn build_open_argv(
    target: &SessionToOpen,
    model: Option<&str>,
    briefing: Option<&str>,
    probe: &dyn ProcessProbe,
    live: &[LiveSession],
) -> Option<Vec<String>> {
    let mut argv = if target.id.is_empty() {
        opener::inplace_argv(None)
    } else {
        opener::decide_inplace_resume(target, probe, live)?.argv
    };
    if let Some(model) = model {
        argv.push("--model".to_string());
        argv.push(model.to_string());
    }
    if let Some(briefing) = briefing {
        argv.push("--append-system-prompt".to_string());
        argv.push(briefing.to_string());
    }
    Some(argv)
}

/// Spawn `target` under `key`, enforcing the no-double-resume guard for a
/// known (non-empty) id — reusing the chōba in-place decision — or
/// skipping it entirely for a fresh (empty-id) spawn, which has no existing
/// session to double-resume. `brigade` wires the launch to banto's own MCP
/// server; a write failure there degrades gracefully (the pre-migration
/// behavior: spawn anyway, without the flag, rather than losing the pane
/// over a config-file write error).
fn execute_open_embedded(
    key: SessionKey,
    target: SessionToOpen,
    brigade: Option<(BrigadeId, MemberToken, BrigadeRole)>,
    model: Option<String>,
    deps: &Deps,
    handles: &mut HashMap<SessionKey, PtyHandle>,
    discovery: &mut Vec<DiscoveryTracker>,
) -> Vec<Event> {
    let claude_home = deps.claude_home;
    // Only read live sessions when a resume might actually need them — a
    // fresh (unresumed) spawn never does.
    let live = if target.id.is_empty() {
        Vec::new()
    } else {
        read_live_sessions(&claude_home.join("sessions"))
    };
    let briefing = brigade
        .as_ref()
        .and_then(|(brigade_id, token, role)| member_briefing(deps, *brigade_id, token, *role));
    let Some(mut argv) = build_open_argv(
        &target,
        model.as_deref(),
        briefing.as_deref(),
        &SysinfoProbe,
        &live,
    ) else {
        return vec![Event::SpawnFailed {
            key,
            error: "already running elsewhere".to_string(),
        }];
    };
    if let Some((brigade_id, token, role)) = &brigade {
        let known_id = (!target.id.is_empty()).then_some(target.id.as_str());
        if let Ok(path) = write_mcp_config(*brigade_id, token, *role, known_id) {
            argv.push("--mcp-config".to_string());
            argv.push(path.to_string_lossy().into_owned());
        }
    }
    // Size is corrected on this same tick's resize pass, once staged.
    match PtyHandle::open(&PortablePtyHost, &argv, Some(&target.cwd), 24, 80) {
        Ok(handle) => {
            let pid = handle.pid();
            handles.insert(key.clone(), handle);
            if key.is_synthetic() {
                discovery.push(DiscoveryTracker {
                    key: key.clone(),
                    cwd: target.cwd,
                    since: SystemTime::now(),
                    member: brigade.map(|(brigade_id, token, _)| (brigade_id, token)),
                    pid,
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

/// Execute one store intent, reusing the store's existing transactional
/// functions, and return the fact it produced.
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
        StoreIntent::JoinGroup { session_id, target } => {
            let mut store = store.borrow_mut();
            let result = match target {
                GroupJoinTargetData::Existing(group_id, name) => store
                    .set_session_group(&SessionId(session_id.clone()), group_id)
                    .map(|()| (group_id, name))
                    .map_err(|err| err.to_string()),
                GroupJoinTargetData::New(name) => match store.create_group(&name) {
                    Ok(group_id) => store
                        .set_session_group(&SessionId(session_id.clone()), group_id)
                        .map(|()| (group_id, name.clone()))
                        .map_err(|err| err.to_string()),
                    Err(err) => Err(format!("failed to create group \"{name}\": {err}")),
                },
            };
            vec![Event::GroupJoinDone { session_id, result }]
        }
        StoreIntent::ResolveMembership { session_id } => {
            // `&mut` (not `&`): healing a member below persists a moved
            // claude_session_id (`set_member_claude_session`), not just
            // reads one.
            let mut store = store.borrow_mut();
            let membership = store
                .brigade_of_claude_session(&SessionId(session_id.clone()))
                .ok()
                .flatten();
            let members = membership.as_ref().map(|(brigade_id, _, _)| {
                store
                    .brigade_members(*brigade_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|member| {
                        let healed_id = heal_member_session(
                            &mut store,
                            *brigade_id,
                            &member.token,
                            member.claude_session_id,
                        );
                        (member.token, member.role, healed_id)
                    })
                    .collect()
            });
            vec![Event::MembershipResolved {
                session_id,
                membership,
                members,
            }]
        }
        StoreIntent::FormBrigade {
            director_row_id,
            name,
            cwd,
            worker_count,
        } => {
            let result = form_brigade_store(store, &director_row_id, &name, worker_count);
            vec![Event::BrigadeFormed {
                director_row_id,
                name,
                cwd,
                result,
            }]
        }
        StoreIntent::AddWorker { brigade_id, cwd } => {
            let result = add_worker_store(store, brigade_id);
            vec![Event::WorkerAdded {
                brigade_id,
                cwd,
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
                        crate::tui::load_hidden_worker_ids(&store),
                        crate::tui::load_directors(&store),
                    )
                });
            vec![Event::Disbanded { brigade_id, result }]
        }
        StoreIntent::SetMemberSession {
            brigade_id,
            token,
            session_id,
        } => {
            let mut store = store.borrow_mut();
            let _ = store.set_member_claude_session(brigade_id, &token, &SessionId(session_id));
            vec![Event::MemberSessionRecorded {
                hidden: crate::tui::load_hidden_worker_ids(&store),
                directors: crate::tui::load_directors(&store),
            }]
        }
    }
}

/// Follow `claude_session_id` to its newest known auto-compaction
/// continuation (`Store::lineage_leaf`) and, if it moved, persist the move
/// (`Store::set_member_claude_session`, v9 move semantics: any other row
/// holding the healed id is cleared) — closing the zombie loop for forks
/// R19's live watcher missed (banto wasn't running when they happened), so
/// re-staging resumes the true continuation instead of a stale ancestor.
/// `None` in, `None` out: a member still awaiting discovery has nothing to
/// heal. Tolerant: a lookup/write failure just leaves the id as recorded,
/// rather than blocking membership resolution over it.
fn heal_member_session(
    store: &mut Store,
    brigade_id: BrigadeId,
    token: &str,
    claude_session_id: Option<SessionId>,
) -> Option<String> {
    let recorded = claude_session_id?;
    let leaf = store
        .lineage_leaf(&recorded)
        .unwrap_or_else(|_| recorded.clone());
    if leaf != recorded {
        let _ = store.set_member_claude_session(brigade_id, token, &leaf);
    }
    Some(leaf.0)
}

/// Create the brigade, its Director row, and `worker_count` Worker rows
/// (schema v7), all-or-nothing. Mirrors the pre-migration `form_brigade`'s
/// store writes, simplified to an atomic outcome rather than continuing past
/// a single worker row's insert failure — an edge case rare enough (same
/// connection, no concurrent writers expected) that the extra per-worker
/// partial-failure bookkeeping isn't worth the complexity this round.
fn form_brigade_store(
    store: &RefCell<Store>,
    director_row_id: &str,
    name: &str,
    worker_count: usize,
) -> Result<(BrigadeId, Vec<MemberToken>), String> {
    let mut store = store.borrow_mut();
    let brigade_id = store.create_brigade(name).map_err(|err| err.to_string())?;
    store
        .add_brigade_member(
            brigade_id,
            "director",
            BrigadeRole::Director,
            Some(&SessionId(director_row_id.to_string())),
        )
        .map_err(|err| err.to_string())?;
    let mut tokens = Vec::new();
    for n in 1..=worker_count {
        let token = format!("worker-{n}");
        store
            .add_brigade_member(brigade_id, &token, BrigadeRole::Worker, None)
            .map_err(|err| err.to_string())?;
        tokens.push(token);
    }
    Ok((brigade_id, tokens))
}

/// Add one more Worker to an already-formed brigade, under the next
/// `worker-N` token.
fn add_worker_store(store: &RefCell<Store>, brigade_id: BrigadeId) -> Result<MemberToken, String> {
    let mut store = store.borrow_mut();
    let members = store
        .brigade_members(brigade_id)
        .map_err(|err| err.to_string())?;
    let next_n = members
        .iter()
        .filter(|member| member.role == BrigadeRole::Worker)
        .count()
        + 1;
    let token = format!("worker-{next_n}");
    store
        .add_brigade_member(brigade_id, &token, BrigadeRole::Worker, None)
        .map_err(|err| err.to_string())?;
    Ok(token)
}

/// Reload the session list from disk. A read failure is tolerated (yields no
/// event, keeping the previous rows) rather than erroring the whole loop out
/// over a transient filesystem hiccup. Also spends this reload's
/// lineage-resolution budget (see [`crate::tui::load_superseded`]).
fn gather_reload(deps: &Deps) -> Vec<Event> {
    let Ok(rows) = session::load_rows(deps.claude_home, deps.thresholds) else {
        return Vec::new();
    };
    let store = deps.store.borrow();
    let rows = crate::tui::exclude_archived(rows, &store);
    let hidden = crate::tui::load_hidden_worker_ids(&store);
    let directors = crate::tui::load_directors(&store);
    let superseded = crate::tui::load_superseded(deps.claude_home, &store, deps.superseded_failed);
    vec![Event::RowsLoaded {
        rows,
        hidden,
        directors,
        superseded,
    }]
}

/// Poll every pending discovery tracker for the id Claude assigned it.
///
/// Two sources, tried in that order:
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
///    batch spawned into one cwd at once — mirrors the pre-migration
///    `discover_new_ids`. The fallback whenever the direct child isn't the
///    `claude` process itself (see `PtyIo::pid`).
///
/// Both skip ids already claimed by an open session or taken earlier in
/// this same pass.
fn poll_discovery(
    trackers: &mut Vec<DiscoveryTracker>,
    provider: &ClaudeCodeProvider,
    claimed: &HashSet<String>,
    live: &[LiveSession],
) -> Vec<Event> {
    let mut used_this_pass: HashSet<String> = HashSet::new();
    let mut resolved: Vec<(usize, String)> = Vec::new();
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
            });
        if let Some(id) = id {
            used_this_pass.insert(id.clone());
            resolved.push((i, id));
        }
    }
    if resolved.is_empty() {
        return Vec::new();
    }
    let events = resolved
        .iter()
        .map(|(i, id)| Event::DiscoveryResult {
            key: trackers[*i].key.clone(),
            session_id: id.clone(),
            member: trackers[*i].member.clone(),
        })
        .collect();
    let resolved_indices: HashSet<usize> = resolved.into_iter().map(|(i, _)| i).collect();
    let mut i = 0;
    trackers.retain(|_| {
        let keep = !resolved_indices.contains(&i);
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

/// Gather this tick's relay observations for the staged brigade's members
/// (unseen messages, live idle/busy status) — the store + live-session reads
/// `engine::update_tick`'s decision logic needs, per member with a known
/// Claude session id and an open pane among the currently-staged ones.
fn gather_relay_observations(
    state: &EmporiumState,
    store: &RefCell<Store>,
    claude_home: &Path,
) -> Vec<RelayObservation> {
    let Stage::Brigade { id, panes, .. } = &state.stage else {
        return Vec::new();
    };
    let brigade_id = *id;
    let members = match store.borrow().brigade_members(brigade_id) {
        Ok(members) => members,
        Err(_) => return Vec::new(),
    };
    let live = read_live_sessions(&claude_home.join("sessions"));
    let mut observations = Vec::new();
    for member in &members {
        let Some(claude_session_id) = member.claude_session_id.as_ref() else {
            continue;
        };
        let key = SessionKey::from_id(&claude_session_id.0);
        if !panes.contains(&key) {
            continue;
        }
        let has_unseen = store
            .borrow()
            .has_unseen_brigade_messages(brigade_id, &member.token, member.role)
            .unwrap_or(false);
        let is_idle_this_tick = live
            .iter()
            .find(|entry| entry.session_id.as_deref() == Some(claude_session_id.0.as_str()))
            .map(|entry| {
                SysinfoProbe.is_alive(entry.pid) && entry.status.as_deref() != Some("busy")
            });
        observations.push(RelayObservation {
            token: member.token.clone(),
            key,
            has_unseen,
            is_idle_this_tick,
        });
    }
    observations
}

/// How many parent-pid hops [`gather_fork_observations`] walks looking for a
/// staged pane's own child pid in a live entry's ancestry — covers `claude`
/// launched through a cmd/npm shim (a couple of hops) with headroom to
/// spare, without letting a pathological chain spin forever.
const FORK_ANCESTRY_DEPTH: u32 = 5;

/// Detect a staged brigade member's Claude session having forked in place.
///
/// Claude Code's AUTO-compaction assigns a session a *new* id while leaving
/// the process itself running (manual `/compact` does not) — the same
/// `sessions/<pid>.json` live-state file the process has always published
/// simply starts reporting a different `sessionId`. One
/// `Event::MemberSessionForked` per staged member whose recorded id no
/// longer matches what its own pane's process is actually reporting; the
/// pid match tries the pane's direct child first, falling back to an
/// ancestry walk for the cmd/npm-shim case (see [`FORK_ANCESTRY_DEPTH`]).
/// Repeats every tick until the store row (and the core's rename it
/// triggers) catch up — `engine::update_member_session_forked` is written
/// to tolerate that.
fn gather_fork_observations(
    state: &EmporiumState,
    store: &RefCell<Store>,
    claude_home: &Path,
    handles: &HashMap<SessionKey, PtyHandle>,
) -> Vec<Event> {
    let Stage::Brigade { id, panes, .. } = &state.stage else {
        return Vec::new();
    };
    let brigade_id = *id;
    let members = match store.borrow().brigade_members(brigade_id) {
        Ok(members) => members,
        Err(_) => return Vec::new(),
    };
    let live = read_live_sessions(&claude_home.join("sessions"));
    let probe = SysinfoProbe;
    let mut events = Vec::new();
    for member in &members {
        let Some(claude_session_id) = member.claude_session_id.as_ref() else {
            continue;
        };
        let old_key = SessionKey::from_id(&claude_session_id.0);
        if !panes.contains(&old_key) {
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
            .any(|entry| entry.session_id.as_deref() == Some(claude_session_id.0.as_str()))
        {
            continue;
        }
        let forked = live.iter().find(|entry| {
            entry
                .session_id
                .as_deref()
                .is_some_and(|id| !id.is_empty() && id != claude_session_id.0)
                && ancestry_reaches(entry.pid, pid, &probe, FORK_ANCESTRY_DEPTH)
        });
        if let Some(new_id) = forked.and_then(|entry| entry.session_id.clone()) {
            events.push(Event::MemberSessionForked {
                brigade_id,
                token: member.token.clone(),
                old_id: claude_session_id.0.clone(),
                new_id,
            });
        }
    }
    events
}

/// This member's role briefing, ready for `--append-system-prompt`, or
/// `None` when the configured template for its role is empty.
///
/// The store read is what keeps the roster honest: `{peers}` has to name
/// the members that exist *at launch*, which is later than any of the
/// core's own decisions about the cell (a Worker added with `b` changes it,
/// and a Director resumed into an existing brigade never went through
/// formation at all).
fn member_briefing(
    deps: &Deps,
    brigade_id: BrigadeId,
    token: &str,
    role: BrigadeRole,
) -> Option<String> {
    let template = deps.brigade.prompt_for(role)?;
    let peers: Vec<String> = deps
        .store
        .borrow()
        .brigade_members(brigade_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|member| member.role != role)
        .map(|member| member.token)
        .collect();
    Some(render_briefing(template, brigade_id, token, &peers))
}

/// Substitute `{brigade}` / `{token}` / `{peers}` into a briefing template.
///
/// Plain string replacement, deliberately: this is banto's own config text
/// being filled in for banto's own launch, not a templating language, and
/// an unrecognized `{...}` is left alone rather than treated as an error —
/// the same leniency the rest of the config layer promises. A member with
/// no addressable peers yet renders as "none yet" rather than an empty gap,
/// so the sentence still reads as a sentence.
fn render_briefing(template: &str, brigade_id: BrigadeId, token: &str, peers: &[String]) -> String {
    let peers = if peers.is_empty() {
        "none yet".to_string()
    } else {
        peers.join(", ")
    };
    template
        .replace("{brigade}", &brigade_id.to_string())
        .replace("{token}", token)
        .replace("{peers}", &peers)
}

/// Write a per-member `--mcp-config` file wiring the embedded claude to
/// banto's own MCP server (`banto _mcp`) with this member's brigade
/// identity, and return its path. Named by `(brigade_id, token)` rather than
/// the Claude session id, since that's the only identity known upfront for a
/// freshly-spawned Worker. Lives under banto's own data dir, never under
/// `~/.claude`.
fn write_mcp_config(
    brigade_id: BrigadeId,
    token: &str,
    role: BrigadeRole,
    claude_session_id: Option<&str>,
) -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let role_token = match role {
        BrigadeRole::Director => "director",
        BrigadeRole::Worker => "worker",
    };
    let mut args = vec![
        "_mcp".to_string(),
        "--brigade".to_string(),
        brigade_id.to_string(),
        "--member".to_string(),
        token.to_string(),
        "--role".to_string(),
        role_token.to_string(),
    ];
    if let Some(session_id) = claude_session_id {
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

fn draw(frame: &mut ratatui::Frame, app: &App, state: &EmporiumState, now: SystemTime) {
    let full_area = frame.area();
    let focus = state.focus;
    let areas = layout(full_area);

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
    view::render_list(frame, app, sidebar_inner, now);

    view::render_summary(frame, app, areas.summary, now);

    let tiles = stage_tiles(areas.pane, &state.stage);
    if tiles.is_empty() {
        let block = Block::bordered()
            .title("session")
            .border_style(border_style(false));
        let content = block.inner(areas.pane);
        frame.render_widget(block, areas.pane);
        frame.render_widget(
            Paragraph::new(
                "Select a session and press Enter.\n\
                 F2 toggles focus · B starts a brigade · q quits.",
            ),
            content,
        );
    } else {
        let focused_key = state.stage.focused_key().cloned();
        for (key, rect) in &tiles {
            let Some(screen) = state.screens.get(key) else {
                continue;
            };
            let focused_tile = focus == Focus::Pane && focused_key.as_ref() == Some(key);
            let block = Block::bordered()
                .title(tile_title(&state.stage, key))
                .border_style(border_style(focused_tile));
            let content = block.inner(*rect);
            frame.render_widget(block, *rect);
            frame.render_widget(Paragraph::new(screen_to_text(screen.screen())), content);
            if focused_tile && !screen.screen().hide_cursor() {
                let (cursor_row, cursor_col) = screen.screen().cursor_position();
                let (x, y) = (content.x + cursor_col, content.y + cursor_row);
                if x < content.x + content.width && y < content.y + content.height {
                    frame.set_cursor_position(Position::new(x, y));
                }
            }
        }
    }

    render_status_bar(
        frame,
        app,
        state.status.as_deref(),
        state.prefix_armed.is_some(),
        areas.status,
    );

    if let Some(modal) = app.modal() {
        banto_tui::render_modal::render_modal(frame, modal, full_area);
    }
}

/// The title shown on a staged tile: its role within a brigade ("director" /
/// "worker N"), or just "session" for a solo pane.
fn tile_title(stage: &Stage, key: &SessionKey) -> String {
    match stage {
        Stage::Brigade { panes, .. } => match panes.iter().position(|k| k == key) {
            Some(0) => "director".to_string(),
            Some(n) => format!("worker {n}"),
            _ => "session".to_string(),
        },
        _ => "session".to_string(),
    }
}

/// Bottom status bar: emporium key hints (or a transient status, or — while
/// a prefix chord is armed — the pending-prefix hint) on the left, the match
/// count on the right.
fn render_status_bar(
    frame: &mut ratatui::Frame,
    app: &App,
    status: Option<&str>,
    prefix_armed: bool,
    area: Rect,
) {
    const NORMAL_HINTS: &str = "j/k move · Enter open · F2 focus · B brigade/disband · b +worker · \
                                F3 pane · / search · n new · d archive · g group · Tab view · \
                                p pin · a hidden · q quit";
    const SEARCH_HINTS: &str = "type to search · Enter confirm · Esc cancel";
    const PREFIX_HINTS: &str =
        "prefix: o/Tab cycle · arrows move · 1-9 pane · b literal · s sidebar · x kill";

    let counts = format!("[{}/{}]", app.filtered_len(), app.total_len());
    let counts_width = counts.chars().count() as u16;
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(counts_width)]).areas(area);

    let (text, color) = if prefix_armed {
        (PREFIX_HINTS.to_string(), Color::Cyan)
    } else {
        match status {
            Some(message) => (message.to_string(), Color::Yellow),
            None => {
                let hints = if app.mode() == Mode::Search {
                    SEARCH_HINTS
                } else {
                    NORMAL_HINTS
                };
                (hints.to_string(), Color::Gray)
            }
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

/// Enables bracketed paste on the HOST terminal (in addition to mouse
/// capture) so a multiline paste arrives as one `Event::Paste` instead of a
/// stream of individual key events. The chōba list TUI (`crate::tui`) is
/// untouched: it has its own, separate `setup_terminal`.
fn setup_terminal() -> Result<Tui> {
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
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

/// Append one line to the diagnostic input log (no-op when disabled).
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
/// keystrokes, pasted text, PTY output chunks, session ids. A real capture
/// necessarily contains real session content; repo invariant 2 ("never
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
                DisableBracketedPaste
            );
            original(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use banto_io::pty::mock::MockPtyHost;

    use super::*;

    fn open(host: &MockPtyHost) -> PtyHandle {
        PtyHandle::open(host, &["child".to_string()], None, 24, 80).unwrap()
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

        // Resolve membership from the OLD id — the exact situation R19's
        // live watcher misses: the fork happened while banto wasn't
        // watching, so nothing ever rekeyed the pane, and the member row
        // still records the ancestor.
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
                .claude_session_id,
            Some(SessionId("w1-new".to_string()))
        );
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

    // --- id discovery: the handle map follows the core's rekey -----------

    /// A stand-in for the placeholder key the core mints for a Worker it
    /// has spawned but Claude hasn't named yet (`SessionKey::new_worker` is
    /// private to the core — only its shape matters here: not a real id).
    fn pending_key(token: &str) -> SessionKey {
        SessionKey::from_id(&format!("new-worker::1::{token}"))
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
        let thresholds = AgeThresholds::default();
        let brigade = BrigadeConfig::default();
        let deps = Deps {
            claude_home: Path::new("/nonexistent"),
            thresholds: &thresholds,
            store: &store,
            superseded_failed: &superseded_failed,
            brigade: &brigade,
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
        let dir = claude_home.join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{pid}.json")),
            format!(
                "{{\"pid\":{pid},\"sessionId\":\"{session_id}\",\"cwd\":{},\"status\":\"idle\"}}",
                serde_json::to_string(&cwd.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_worker_that_has_written_no_session_file_is_still_identified_by_its_pid() {
        // The dogfooding bug: `claude` writes a session's jsonl at its first
        // turn, but publishes `sessions/<pid>.json` at startup. A brigade
        // Worker nobody has typed into yet therefore has an id but no
        // session file — invisible to the file scan forever, so its store
        // row stayed NULL and every re-stage of the cell spawned it again
        // as a brand-new session.
        let claude_home = tempfile::tempdir().unwrap();
        let cwd = PathBuf::from("/work/alpha");
        let provider = ClaudeCodeProvider::new(claude_home.path().to_path_buf());
        write_live_state(claude_home.path(), 4242, "w1", &cwd);
        let live = read_live_sessions(&claude_home.path().join("sessions"));

        let mut trackers = vec![DiscoveryTracker {
            key: pending_key("worker-1"),
            cwd: cwd.clone(),
            since: SystemTime::now(),
            member: Some((1, "worker-1".to_string())),
            pid: Some(4242),
        }];

        let events = poll_discovery(&mut trackers, &provider, &HashSet::new(), &live);

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
        let provider = ClaudeCodeProvider::new(claude_home.path().to_path_buf());
        write_live_state(
            claude_home.path(),
            4242,
            "someone-elses-session",
            Path::new("/work/beta"),
        );
        let live = read_live_sessions(&claude_home.path().join("sessions"));

        let mut trackers = vec![DiscoveryTracker {
            key: pending_key("worker-1"),
            cwd: PathBuf::from("/work/alpha"),
            since: SystemTime::now(),
            member: Some((1, "worker-1".to_string())),
            pid: Some(4242),
        }];

        assert!(
            poll_discovery(&mut trackers, &provider, &HashSet::new(), &live).is_empty(),
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
        let provider = ClaudeCodeProvider::new(claude_home.path().to_path_buf());
        let since = SystemTime::now() - Duration::from_secs(1);
        // `pid: None` is the fallback shape this test is about: no usable
        // child pid, so only the session-file scan can answer.
        let tracker = |token: &str| DiscoveryTracker {
            key: pending_key(token),
            cwd: cwd.clone(),
            since,
            member: Some((1, token.to_string())),
            pid: None,
        };
        let mut trackers = vec![tracker("worker-1"), tracker("worker-2")];

        // Pass 1: only the first Worker's session file exists yet.
        write_session_at(claude_home.path(), "w1", &cwd);
        let mut claimed: HashSet<String> = trackers
            .iter()
            .map(|tracker| tracker.key.as_str().to_string())
            .collect();
        let events = poll_discovery(&mut trackers, &provider, &claimed, &[]);
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
        let events = poll_discovery(&mut trackers, &provider, &claimed, &[]);
        assert!(
            matches!(
                events.as_slice(),
                [Event::DiscoveryResult { session_id, .. }] if session_id == "w2"
            ),
            "worker-2 must take the unclaimed id, not re-take worker-1's: {events:?}"
        );
        assert!(trackers.is_empty());
    }

    // --- compact-fork tracking: gather_fork_observations ------------------

    /// A staged brigade of a Director plus one Worker, and a matching store
    /// row for the Worker holding `worker_session_id` — the shape
    /// `gather_fork_observations` reads.
    fn store_with_staged_worker(worker_session_id: &str) -> (RefCell<Store>, EmporiumState) {
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

        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Brigade {
            id: brigade_id,
            panes: vec![
                SessionKey::from_id("dir"),
                SessionKey::from_id(worker_session_id),
            ],
            focused: 0,
        };
        (RefCell::new(store), state)
    }

    #[test]
    fn gather_fork_observations_detects_a_forked_worker_by_exact_pid_match() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state) = store_with_staged_worker("w1-old");
        write_live_state(claude_home.path(), 4242, "w1-new", Path::new("/work/alpha"));

        let mut handles = HashMap::new();
        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        handles.insert(SessionKey::from_id("w1-old"), open(&host));

        let events = gather_fork_observations(&state, &store, claude_home.path(), &handles);

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
    fn gather_fork_observations_is_silent_when_the_live_entry_still_reports_the_recorded_id() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state) = store_with_staged_worker("w1");
        write_live_state(claude_home.path(), 4242, "w1", Path::new("/work/alpha"));

        let mut handles = HashMap::new();
        let host = MockPtyHost {
            pid: Some(4242),
            ..Default::default()
        };
        handles.insert(SessionKey::from_id("w1"), open(&host));

        let events = gather_fork_observations(&state, &store, claude_home.path(), &handles);

        assert!(events.is_empty(), "no fork happened: {events:?}");
    }

    #[test]
    fn gather_fork_observations_ignores_a_pane_with_no_known_child_pid() {
        let claude_home = tempfile::tempdir().unwrap();
        let (store, state) = store_with_staged_worker("w1-old");
        write_live_state(claude_home.path(), 4242, "w1-new", Path::new("/work/alpha"));

        // No handle registered at all for the pane's key, so its pid can
        // never be known — this must not panic, just find nothing.
        let handles = HashMap::new();

        let events = gather_fork_observations(&state, &store, claude_home.path(), &handles);

        assert!(events.is_empty());
    }

    // --- worker model on resume: build_open_argv --------------------------

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
            title: "Fix login".to_string(),
            cwd: PathBuf::from("/work/alpha"),
        }
    }

    #[test]
    fn build_open_argv_appends_model_to_a_resumed_sessions_argv() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let argv =
            build_open_argv(&open_target("sess-1"), Some("opus"), None, &probe, &[]).unwrap();
        assert_eq!(
            argv,
            ["claude", "--resume", "sess-1", "--model", "opus"].map(str::to_string)
        );
    }

    #[test]
    fn build_open_argv_appends_model_to_a_fresh_launchs_argv_too() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let argv = build_open_argv(&open_target(""), Some("opus"), None, &probe, &[]).unwrap();
        assert_eq!(argv, ["claude", "--model", "opus"].map(str::to_string));
    }

    #[test]
    fn build_open_argv_omits_model_when_none() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let argv = build_open_argv(&open_target("sess-1"), None, None, &probe, &[]).unwrap();
        assert_eq!(argv, ["claude", "--resume", "sess-1"].map(str::to_string));
    }

    #[test]
    fn build_open_argv_is_none_when_the_resume_is_refused() {
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
        }];
        assert!(
            build_open_argv(&open_target("sess-1"), Some("opus"), None, &probe, &live).is_none()
        );
    }

    // --- role briefings (--append-system-prompt) -------------------------

    #[test]
    fn build_open_argv_appends_the_briefing_after_the_model() {
        let probe = MockProbe {
            alive: HashSet::new(),
        };
        let argv = build_open_argv(
            &open_target("sess-1"),
            Some("opus"),
            Some("you are the Director"),
            &probe,
            &[],
        )
        .unwrap();
        assert_eq!(
            argv,
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
    fn render_briefing_substitutes_the_brigade_the_token_and_the_peer_roster() {
        let peers = ["worker-1".to_string(), "worker-2".to_string()];
        assert_eq!(
            render_briefing("{token} of {brigade} leads {peers}", 7, "director", &peers),
            "director of 7 leads worker-1, worker-2"
        );
    }

    #[test]
    fn render_briefing_says_none_yet_rather_than_leaving_a_gap() {
        assert_eq!(
            render_briefing("your peers: {peers}.", 1, "director", &[]),
            "your peers: none yet."
        );
    }

    #[test]
    fn render_briefing_leaves_an_unknown_placeholder_alone() {
        // Lenient like the rest of the config layer: a typo in the operator's
        // own template is not worth failing a launch over.
        assert_eq!(
            render_briefing("{brigade}/{nope}", 3, "director", &[]),
            "3/{nope}"
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
        let superseded_failed = RefCell::new(HashSet::new());
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
        let deps = |brigade| Deps {
            claude_home: Path::new("/nonexistent"),
            thresholds: &thresholds,
            store: &store,
            superseded_failed: &superseded_failed,
            brigade,
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
            &Event::Tick { relay: vec![] },
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
            &Event::Tick { relay: vec![] },
            run_start - Duration::from_secs(1),
        );
        drop(recorder);

        let text = std::fs::read_to_string(&path).unwrap();
        let events = banto_core::replay::parse_stream(&text).unwrap();
        assert_eq!(events[0].offset_ms, 0);
    }
}
