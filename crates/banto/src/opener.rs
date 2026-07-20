//! Bin-level wiring between `banto-core`'s `Opener`/`Store` and the resume
//! flow triggered from the TUI.
//!
//! Decides whether a session's existing pane is still alive (focus it) or a
//! new one must be spawned (open it and record where it landed), enforcing
//! the "never resume a session twice" invariant (CLAUDE.md invariant 4;
//! docs/REQUIREMENTS.md "Opener spec"). Liveness is judged purely by PID (the
//! `banto _wrap` process registers its own PID once it starts — see
//! [`crate::wrap`] — mirroring how `banto_core::status` judges session
//! activity), never by querying the terminal backend itself.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use banto_core::config::OpenerMode;
use banto_core::model::SessionId;
use banto_core::opener::{
    self, Backend, CommandRunner, CommandSpec, OpenError, Opener, ResumeCommand, SessionHandle,
    TmuxOpener, WindowsTerminalOpener,
};
use banto_core::status::{LiveSession, ProcessProbe};
use banto_core::store::{PaneRecord, Store, StoreError};

/// A session about to be opened or focused.
pub struct SessionToOpen {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
}

/// Outcome of an open/focus attempt, for the caller to turn into a status message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOutcome {
    /// An existing pane was brought to the front.
    Focused,
    /// A new pane/tab was created.
    Opened,
    /// A pane exists and is (presumed) alive, but this backend cannot focus
    /// an existing pane (Windows Terminal). Refuses to open a second one.
    AlreadyOpenCannotFocus,
    /// No pane record exists for this session (so there's nothing to focus),
    /// but its own live-state file shows it's still running — most likely a
    /// session launched via the `n` new-session modal, which has no
    /// pre-existing session id to key a pane record against. Refuses to
    /// `--resume` it, since that would fork its history (CLAUDE.md
    /// invariant 4) while it's already open somewhere banto can't locate.
    AlreadyRunningUntracked,
    /// No backend could be determined (`OpenerMode::Auto` with neither
    /// `$TMUX` nor `$WT_SESSION` set).
    NoBackendDetected,
}

/// Errors from [`open_session`].
#[derive(Debug, thiserror::Error)]
pub enum SessionOpenError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Open(#[from] OpenError),
}

/// Resolve the backend to open new sessions with. `Auto` detects it from the
/// environment via `env` (injected so this stays deterministic in tests);
/// production code passes `std::env::var`.
pub fn resolve_backend(mode: OpenerMode, env: impl Fn(&str) -> Option<String>) -> Option<Backend> {
    match mode {
        OpenerMode::Auto => opener::detect_backend(env),
        OpenerMode::Psmux => Some(Backend::Psmux),
        OpenerMode::WindowsTerminal => Some(Backend::WindowsTerminal),
    }
}

/// Launch a brand-new (non-resumed) `claude` session in `cwd`, via `banto
/// _wrap --new-session` so it can later be found and focused.
///
/// Unlike [`open_session`], there is no pre-existing session id to key a
/// double-resume check or a pane-map record against here in the opener —
/// starting two new sessions is not a double resume, it's just two new
/// sessions — so this skips the store entirely. But the session Claude
/// creates for `cwd` *is* discovered and recorded, just not by this
/// function: `_wrap --new-session` (`crate::wrap::run_new_session`) does
/// that itself once it's running inside the new pane, since only it can
/// observe both where it landed and which session id eventually shows up.
///
/// `wrap_log`, when given, is the caller's own resolved `BANTO_WRAP_LOG`
/// value, passed through to `_wrap` via `--wrap-log` (see
/// [`new_session_wrap_argv`]) rather than left for it to read from its own
/// environment, which a psmux-spawned process does not reliably inherit.
pub fn open_new_session<R: CommandRunner + 'static>(
    backend: Option<Backend>,
    cwd: &Path,
    runner: R,
    anchor_pane: Option<&str>,
    wrap_log: Option<&str>,
) -> Result<OpenOutcome, OpenError> {
    let Some(backend) = backend else {
        return Ok(OpenOutcome::NoBackendDetected);
    };
    let title = cwd
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.display().to_string());
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string));
    let cmd = ResumeCommand {
        // Unused: there is no pane-map record for this launch to key.
        session_id: String::new(),
        argv: new_session_wrap_argv(exe.as_deref(), cwd, backend, wrap_log),
        cwd: cwd.to_path_buf(),
        title,
    };
    let opener: Box<dyn Opener> = match (backend, anchor_pane) {
        (Backend::Psmux, Some(anchor)) => {
            Box::new(TmuxOpener::new(runner).with_anchor_pane(anchor))
        }
        (Backend::Psmux, None) => Box::new(TmuxOpener::new(runner)),
        (Backend::WindowsTerminal, _) => Box::new(WindowsTerminalOpener::new(runner)),
    };
    opener.open(&cmd)?;
    Ok(OpenOutcome::Opened)
}

/// Open or focus `session`, enforcing the no-double-resume invariant.
///
/// `backend` is the resolved opener backend for *new* opens (see
/// [`resolve_backend`]); an existing pane is always focused through whichever
/// backend it was originally opened with, regardless of the current default.
/// `live` is the current `<claude_home>/sessions/*.json` snapshot (see
/// `banto_core::status::read_live_sessions`), used only for the untracked
/// case below.
pub fn open_session<R: CommandRunner + Clone + 'static>(
    store: &Store,
    probe: &dyn ProcessProbe,
    backend: Option<Backend>,
    session: &SessionToOpen,
    runner: R,
    anchor_pane: Option<&str>,
    live: &[LiveSession],
) -> Result<OpenOutcome, SessionOpenError> {
    let id = SessionId(session.id.clone());

    match store.get_pane(&id)? {
        Some(record) => {
            // No PID yet means the wrapper is still starting up; assume alive
            // rather than risk a double resume.
            let alive = record.pid.is_none_or(|pid| probe.is_alive(pid));
            if alive {
                match focus_existing(store, &record, runner.clone())? {
                    FocusResult::Outcome(outcome) => return Ok(outcome),
                    // The backend CLI ran but reported the pane doesn't
                    // exist any more (already cleaned up); fall through to
                    // open fresh.
                    FocusResult::Stale => {}
                }
            } else {
                // The wrapper is gone but never cleaned up (e.g. it crashed).
                store.remove_pane(&id)?;
            }
        }
        // No pane record — but the session's own live-state file shows it's
        // still running (e.g. it was launched via the `n` new-session
        // modal, which has no pre-existing session id to key a pane record
        // against). Opening fresh here would `--resume` an already-running
        // session, forking its history.
        None if is_live(&session.id, live, probe) => {
            return Ok(OpenOutcome::AlreadyRunningUntracked);
        }
        None => {}
    }

    open_fresh(store, backend, session, runner, anchor_pane)
}

/// Whether any live-state entry names `session_id` with a currently-alive
/// pid, regardless of busy/idle status — used only to guard against
/// `--resume`-ing (and thereby history-forking) a session that's live but
/// untracked (see [`open_session`]). Unlike `banto_core::status::classify`
/// (which is about which activity dot to show, so a busy entry outranks a
/// merely-alive one), this only answers "is this session actually running
/// right now".
fn is_live(session_id: &str, live: &[LiveSession], probe: &dyn ProcessProbe) -> bool {
    live.iter()
        .any(|entry| entry.session_id.as_deref() == Some(session_id) && probe.is_alive(entry.pid))
}

/// Outcome of [`focus_existing`]: either a final [`OpenOutcome`], or a
/// signal that the pane record was stale and the caller should open fresh.
enum FocusResult {
    Outcome(OpenOutcome),
    Stale,
}

fn focus_existing<R: CommandRunner + 'static>(
    store: &Store,
    record: &PaneRecord,
    runner: R,
) -> Result<FocusResult, SessionOpenError> {
    let Some(backend) = parse_backend_key(&record.backend) else {
        // Unknown backend string (e.g. written by a future banto version).
        store.remove_pane(&record.session_id)?;
        return Ok(FocusResult::Outcome(OpenOutcome::NoBackendDetected));
    };
    let Some(handle) = decode_handle(backend, &record.target) else {
        store.remove_pane(&record.session_id)?;
        return Ok(FocusResult::Outcome(OpenOutcome::NoBackendDetected));
    };

    match opener::opener_for(backend, runner).focus(&handle) {
        Ok(()) => Ok(FocusResult::Outcome(OpenOutcome::Focused)),
        Err(OpenError::UnsupportedFocus { .. }) => {
            Ok(FocusResult::Outcome(OpenOutcome::AlreadyOpenCannotFocus))
        }
        Err(OpenError::Command { .. }) => {
            // The backend CLI ran but failed (e.g. psmux's `select-window`
            // reporting no such window): the pane is actually gone even
            // though our record didn't reflect it yet. Stale, not a real
            // error — clean up and let the caller open a fresh one.
            store.remove_pane(&record.session_id)?;
            Ok(FocusResult::Stale)
        }
        Err(err) => Err(err.into()),
    }
}

fn open_fresh<R: CommandRunner + 'static>(
    store: &Store,
    backend: Option<Backend>,
    session: &SessionToOpen,
    runner: R,
    anchor_pane: Option<&str>,
) -> Result<OpenOutcome, SessionOpenError> {
    let Some(backend) = backend else {
        return Ok(OpenOutcome::NoBackendDetected);
    };

    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string));
    let cmd = ResumeCommand {
        session_id: session.id.clone(),
        argv: wrap_argv(exe.as_deref(), &session.id),
        cwd: session.cwd.clone(),
        title: session.title.clone(),
    };
    // Anchor psmux splits on banto's own pane (resolved from TMUX_PANE by
    // the caller) so the resume pane lands next to banto instead of in
    // whatever window the user's client is currently focused on.
    let opener: Box<dyn Opener> = match (backend, anchor_pane) {
        (Backend::Psmux, Some(anchor)) => {
            Box::new(TmuxOpener::new(runner).with_anchor_pane(anchor))
        }
        (Backend::Psmux, None) => Box::new(TmuxOpener::new(runner)),
        (Backend::WindowsTerminal, _) => Box::new(WindowsTerminalOpener::new(runner)),
    };
    let handle = opener.open(&cmd)?;

    store.set_pane(&PaneRecord {
        session_id: SessionId(session.id.clone()),
        backend: backend_key(backend).to_string(),
        target: encode_target(&handle),
        // Not yet known: `banto _wrap` registers its own PID once it starts
        // (see `crate::wrap::register_pid`).
        pid: None,
        opened_at: SystemTime::now(),
    })?;

    Ok(OpenOutcome::Opened)
}

/// Build the `<banto> _wrap --session <id> -- claude --resume <id>` argv.
///
/// `exe` is the resolved path to the running `banto` binary (production
/// callers pass `std::env::current_exe()`, injected here so this stays
/// deterministic in tests) so the spawned pane/tab can find it even when it
/// isn't on `$PATH` (e.g. a dev build); `None` falls back to the bare name
/// `banto` (relies on `$PATH`).
fn wrap_argv(exe: Option<&str>, session_id: &str) -> Vec<String> {
    let banto_exe = exe.unwrap_or("banto").to_string();
    vec![
        banto_exe,
        "_wrap".to_string(),
        "--session".to_string(),
        session_id.to_string(),
        "--".to_string(),
        "claude".to_string(),
        "--resume".to_string(),
        session_id.to_string(),
    ]
}

/// Build the `<banto> _wrap --new-session --cwd <cwd> --backend <key>
/// [--wrap-log <path>] -- claude` argv (see [`wrap_argv`] for the
/// resume-mode equivalent and the `exe` convention). Unlike the resume
/// case, `_wrap` itself discovers the session id Claude assigns and writes
/// its own pane record (`crate::wrap::run_new_session`) — there's no id
/// known yet for this process to be told. `backend` is passed through
/// explicitly (this function's own already-resolved choice) rather than
/// left for `_wrap` to re-detect from its environment — see
/// `crate::wrap::resolve_own_pane`'s doc comment for why that would be
/// unreliable.
///
/// `wrap_log`, when set, is banto's own `BANTO_WRAP_LOG` value, passed
/// through explicitly for the same kind of reason: a psmux-spawned `_wrap`
/// process does not reliably inherit banto's own environment
/// (docs/notes/psmux-spike.md), so relying on `_wrap` to read
/// `BANTO_WRAP_LOG` from its own environment would silently produce no
/// diagnostic log at all — see `crate::wrap::WrapLog::new`'s doc comment.
fn new_session_wrap_argv(
    exe: Option<&str>,
    cwd: &Path,
    backend: Backend,
    wrap_log: Option<&str>,
) -> Vec<String> {
    let banto_exe = exe.unwrap_or("banto").to_string();
    let mut argv = vec![
        banto_exe,
        "_wrap".to_string(),
        "--new-session".to_string(),
        "--cwd".to_string(),
        cwd.display().to_string(),
        "--backend".to_string(),
        backend_key(backend).to_string(),
    ];
    if let Some(path) = wrap_log {
        argv.push("--wrap-log".to_string());
        argv.push(path.to_string());
    }
    argv.push("--".to_string());
    argv.push("claude".to_string());
    argv
}

/// Stable string stored in [`PaneRecord::backend`] (kebab-case, independent
/// of `Backend`'s human-readable `Display`). `pub(crate)`: also used by
/// `crate::wrap`'s new-session flow, which writes its own pane record
/// directly (there's no pre-existing one from an `open_*` call for it to
/// attach to).
pub(crate) fn backend_key(backend: Backend) -> &'static str {
    match backend {
        Backend::Psmux => "psmux",
        Backend::WindowsTerminal => "windows-terminal",
    }
}

/// Inverse of [`backend_key`]. `pub(crate)`: also used by `main`'s `_wrap
/// --new-session --backend <key>` parsing.
pub(crate) fn parse_backend_key(key: &str) -> Option<Backend> {
    match key {
        "psmux" => Some(Backend::Psmux),
        "windows-terminal" => Some(Backend::WindowsTerminal),
        _ => None,
    }
}

/// Encode a [`SessionHandle`] into [`PaneRecord::target`]. `pub(crate)`: see
/// [`backend_key`].
pub(crate) fn encode_target(handle: &SessionHandle) -> String {
    match handle {
        SessionHandle::Tmux {
            session,
            window_id,
            pane_id,
        } => format!("{session}:{window_id}:{pane_id}"),
        SessionHandle::WindowsTerminal => String::new(),
    }
}

/// Inverse of [`encode_target`].
///
/// A pane record written before psmux's non-unique-id finding
/// (docs/notes/psmux-spike.md, 2026-07-20) encodes the old, un-qualified
/// `<window_id>:<pane_id>` form (two `:`-joined parts). That target can no
/// longer be resolved to a specific session, so it decodes to `None` here —
/// the caller ([`focus_existing`]) treats a `None` decode as a stale record
/// and opens a fresh pane instead of risking an ambiguous focus.
fn decode_handle(backend: Backend, target: &str) -> Option<SessionHandle> {
    match backend {
        Backend::Psmux => {
            let mut parts = target.splitn(3, ':');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(session), Some(window_id), Some(pane_id))
                    if !session.is_empty() && !window_id.is_empty() && !pane_id.is_empty() =>
                {
                    Some(SessionHandle::Tmux {
                        session: session.to_string(),
                        window_id: window_id.to_string(),
                        pane_id: pane_id.to_string(),
                    })
                }
                _ => None,
            }
        }
        Backend::WindowsTerminal => Some(SessionHandle::WindowsTerminal),
    }
}

/// Resolve banto's own session-qualified anchor pane (`"<session>:<pane>"`)
/// for a psmux split, so a resume/new-session pane always lands next to
/// banto's own pane unambiguously — psmux reuses pane/window ids across
/// sessions (docs/notes/psmux-spike.md), so a bare pane id is no longer
/// enough to anchor a split correctly.
///
/// `backend` is the caller's own already-resolved choice, trusted rather
/// than guessed from `tmux_pane`'s presence (mirrors
/// `crate::wrap::resolve_own_pane`'s reasoning: banto can be running inside
/// an unrelated psmux session while still configured to open new sessions
/// via Windows Terminal). `tmux_pane` is banto's own bare `$TMUX_PANE` (e.g.
/// `%2`); `None` (not running inside psmux at all) always yields `None`.
///
/// If session resolution itself fails (the `display-message` call errors,
/// or banto somehow isn't attached to a psmux session despite `$TMUX_PANE`
/// being set), falls back to the bare pane id — best-effort, matching the
/// pre-session-qualification behavior — rather than refusing to anchor the
/// split at all.
pub(crate) fn resolve_own_anchor<R: CommandRunner>(
    backend: Option<Backend>,
    runner: &R,
    tmux_pane: Option<&str>,
) -> Option<String> {
    if backend != Some(Backend::Psmux) {
        return None;
    }
    let pane = tmux_pane?;
    match resolve_own_session(runner) {
        Some(session) => Some(format!("{session}:{pane}")),
        None => Some(pane.to_string()),
    }
}

/// Resolve banto's own psmux session name via `display-message -p
/// '#{session_name}'` (no `-t`, so it targets banto's own pane — the
/// reliable route per docs/notes/psmux-spike.md; the `$TMUX` env var's
/// session-id field is explicitly documented there as unreliable).
fn resolve_own_session<R: CommandRunner>(runner: &R) -> Option<String> {
    let spec = CommandSpec::new(
        "psmux",
        ["display-message", "-p", "#{session_name}"].map(str::to_string),
    );
    let output = runner.run(&spec).ok()?;
    if !output.success {
        return None;
    }
    let session = output.stdout.trim();
    (!session.is_empty()).then(|| session.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::opener::{CommandOutput, CommandSpec};
    use std::cell::{Cell, RefCell};
    use std::collections::{HashSet, VecDeque};
    use std::rc::Rc;

    /// Records every command and always reports success with a fixed
    /// session-qualified `create` output for commands that need to parse
    /// one (psmux's `-P -F` create format, now `<session>:<window>:<pane>`
    /// — docs/notes/psmux-spike.md, 2026-07-20) — unless configured via
    /// [`MockRunner::failing_select_pane`] to simulate the backend CLI
    /// reporting the pane no longer exists. Cheaply `Clone`-able (shared
    /// call log) so a clone can be moved into `open_session` while the
    /// original is kept around to inspect the calls afterward.
    #[derive(Clone, Default)]
    struct MockRunner {
        calls: Rc<RefCell<Vec<CommandSpec>>>,
        fail_select_pane: Rc<Cell<bool>>,
        /// Queued responses consumed before falling back to the default
        /// create-format success (see [`MockRunner::with_responses`]).
        responses: Rc<RefCell<VecDeque<CommandOutput>>>,
    }

    impl MockRunner {
        fn calls(&self) -> Vec<CommandSpec> {
            self.calls.borrow().clone()
        }

        /// A runner whose focus-time `select-pane` call fails, simulating
        /// psmux reporting the pane is gone (e.g. it was closed out of
        /// band). `focus` (`TmuxOpener::focus`) only ever calls
        /// `select-pane -t <target>` now — never
        /// `select-window`/`switch-client` (docs/notes/psmux-spike.md,
        /// 2026-07-20) — so that 3-arg form (no `-T`) is the only call this
        /// fails; `open`'s own tagging `select-pane -t <target> -T <title>`
        /// call (used when this runner also serves a fallback fresh-open)
        /// is left alone, distinguished by the presence of `-T`.
        fn failing_select_pane() -> Self {
            let runner = Self::default();
            runner.fail_select_pane.set(true);
            runner
        }

        /// A runner returning `responses` in order for its first calls (used
        /// to test [`resolve_own_anchor`] in isolation from the psmux
        /// create-format response below), then falling back to the default
        /// behavior once exhausted.
        fn with_target_responses(responses: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                responses: Rc::new(RefCell::new(responses.into_iter().collect())),
                ..Self::default()
            }
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, OpenError> {
            self.calls.borrow_mut().push(spec.clone());
            if let Some(response) = self.responses.borrow_mut().pop_front() {
                return Ok(response);
            }
            let is_focus_select_pane = spec.args.first().map(String::as_str) == Some("select-pane")
                && !spec.args.iter().any(|a| a == "-T");
            if self.fail_select_pane.get() && is_focus_select_pane {
                return Ok(CommandOutput::failure(Some(1), "no such pane".to_string()));
            }
            Ok(CommandOutput::success("play:@1:%1\n"))
        }
    }

    struct MockProbe {
        alive: HashSet<u32>,
    }

    impl MockProbe {
        fn with_alive(pids: &[u32]) -> Self {
            Self {
                alive: pids.iter().copied().collect(),
            }
        }
    }

    impl ProcessProbe for MockProbe {
        fn is_alive(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }
    }

    fn session() -> SessionToOpen {
        SessionToOpen {
            id: "sess-1".to_string(),
            title: "Fix login".to_string(),
            cwd: PathBuf::from("/work/alpha"),
        }
    }

    #[test]
    fn opens_fresh_and_records_the_pane_when_nothing_is_tracked() {
        let store = Store::open_in_memory().unwrap();
        let probe = MockProbe::with_alive(&[]);
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            &probe,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &[],
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let record = store
            .get_pane(&SessionId("sess-1".to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(record.backend, "psmux");
        assert_eq!(record.target, "play:@1:%1");
        assert_eq!(record.pid, None);
    }

    /// A synthetic live-state entry, e.g. a session launched via the `n`
    /// new-session modal (no pane record) that's still running.
    fn live_entry(session_id: &str, pid: u32) -> LiveSession {
        LiveSession {
            pid,
            session_id: Some(session_id.to_string()),
            cwd: None,
            status: None,
            kind: None,
            name: None,
        }
    }

    #[test]
    fn is_live_matches_only_a_live_entry_with_the_right_session_id_and_a_live_pid() {
        let probe = MockProbe::with_alive(&[100]);

        assert!(is_live("sess-1", &[live_entry("sess-1", 100)], &probe));
        // Wrong session id.
        assert!(!is_live("sess-1", &[live_entry("sess-2", 100)], &probe));
        // Right session id, dead pid.
        assert!(!is_live("sess-1", &[live_entry("sess-1", 999)], &probe));
        // No live entries at all.
        assert!(!is_live("sess-1", &[], &probe));
    }

    #[test]
    fn live_but_untracked_session_refuses_to_open_fresh_to_avoid_a_double_resume() {
        let store = Store::open_in_memory().unwrap();
        let probe = MockProbe::with_alive(&[4242]);
        let runner = MockRunner::default();
        let live = [live_entry("sess-1", 4242)];

        let outcome = open_session(
            &store,
            &probe,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &live,
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::AlreadyRunningUntracked);
        // Never attempted to open a second pane, and never recorded one.
        assert!(runner.calls().is_empty());
        assert_eq!(
            store.get_pane(&SessionId("sess-1".to_string())).unwrap(),
            None
        );
    }

    #[test]
    fn untracked_session_with_no_matching_live_entry_opens_fresh_as_before() {
        let store = Store::open_in_memory().unwrap();
        // A live entry exists, but for a different session — and one for
        // this session's id whose pid is dead. Neither should block a fresh
        // open.
        let probe = MockProbe::with_alive(&[100]);
        let runner = MockRunner::default();
        let live = [
            live_entry("some-other-session", 100),
            live_entry("sess-1", 999), // dead pid
        ];

        let outcome = open_session(
            &store,
            &probe,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &live,
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
    }

    #[test]
    fn open_fresh_anchors_the_split_on_the_given_pane() {
        let store = Store::open_in_memory().unwrap();
        let probe = MockProbe::with_alive(&[]);
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            &probe,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            Some("%7"),
            &[],
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let calls = runner.calls();
        let args = &calls[0].args;
        assert_eq!(args[0], "split-window");
        let t = args.iter().position(|a| a == "-t").expect("-t missing");
        assert_eq!(args[t + 1], "%7");
    }

    #[test]
    fn no_backend_detected_opens_nothing() {
        let store = Store::open_in_memory().unwrap();
        let probe = MockProbe::with_alive(&[]);
        let runner = MockRunner::default();

        let outcome =
            open_session(&store, &probe, None, &session(), runner.clone(), None, &[]).unwrap();

        assert_eq!(outcome, OpenOutcome::NoBackendDetected);
        assert!(runner.calls().is_empty());
        assert_eq!(
            store.get_pane(&SessionId("sess-1".to_string())).unwrap(),
            None
        );
    }

    #[test]
    fn focuses_existing_pane_when_pid_is_alive() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "psmux".to_string(),
                target: "play:@3:%8".to_string(),
                pid: Some(100),
                opened_at: SystemTime::now(),
            })
            .unwrap();
        let probe = MockProbe::with_alive(&[100]);
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            &probe,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &[],
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Focused);
        let calls = runner.calls();
        // Only a session-qualified `select-pane` (docs/notes/psmux-spike.md,
        // 2026-07-20): no `select-window` (fails on psmux for `session:@id`
        // targets) and no `switch-client` (corrupted the live server).
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args, vec!["select-pane", "-t", "play:%8"]);
    }

    #[test]
    fn missing_pid_is_treated_as_alive_to_avoid_double_resume() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "psmux".to_string(),
                target: "play:@3:%8".to_string(),
                pid: None,
                opened_at: SystemTime::now(),
            })
            .unwrap();
        let probe = MockProbe::with_alive(&[]);
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            &probe,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &[],
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Focused);
    }

    #[test]
    fn dead_pid_is_stale_and_opens_fresh() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "psmux".to_string(),
                target: "play:@3:%8".to_string(),
                pid: Some(999),
                opened_at: SystemTime::now(),
            })
            .unwrap();
        let probe = MockProbe::with_alive(&[]); // 999 is dead
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            &probe,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &[],
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let record = store
            .get_pane(&SessionId("sess-1".to_string()))
            .unwrap()
            .unwrap();
        // Replaced with the freshly opened pane, not the stale one.
        assert_eq!(record.target, "play:@1:%1");
    }

    #[test]
    fn focus_command_error_is_treated_as_stale_and_opens_fresh() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "psmux".to_string(),
                target: "play:@3:%8".to_string(),
                pid: Some(100),
                opened_at: SystemTime::now(),
            })
            .unwrap();
        // Pid alive, so a normal focus would be attempted, but the backend
        // reports the pane itself is gone (e.g. closed out of band).
        let probe = MockProbe::with_alive(&[100]);
        let runner = MockRunner::failing_select_pane();

        let outcome = open_session(
            &store,
            &probe,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &[],
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let record = store
            .get_pane(&SessionId("sess-1".to_string()))
            .unwrap()
            .unwrap();
        // Replaced with the freshly opened pane, not the stale one.
        assert_eq!(record.target, "play:@1:%1");
        assert_ne!(record.target, "play:@3:%8");
    }

    #[test]
    fn windows_terminal_alive_session_refuses_a_second_open() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "windows-terminal".to_string(),
                target: String::new(),
                pid: Some(100),
                opened_at: SystemTime::now(),
            })
            .unwrap();
        let probe = MockProbe::with_alive(&[100]);
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            &probe,
            Some(Backend::WindowsTerminal),
            &session(),
            runner.clone(),
            None,
            &[],
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::AlreadyOpenCannotFocus);
        // Never attempted to open a second tab.
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn open_new_session_wraps_claude_via_wrap_new_session_with_no_session_id() {
        let runner = MockRunner::default();

        let outcome = open_new_session(
            Some(Backend::Psmux),
            &PathBuf::from("/work/alpha"),
            runner.clone(),
            None,
            None,
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let calls = runner.calls();
        // Wrapped via `_wrap --new-session --cwd ... -- claude` (no
        // `--session <id>`: there's no pre-existing session id to tell it).
        assert!(calls[0].args.iter().any(|a| a == "_wrap"));
        assert!(calls[0].args.iter().any(|a| a == "--new-session"));
        assert!(!calls[0].args.iter().any(|a| a == "--session"));
        // No `BANTO_WRAP_LOG` was resolved, so no `--wrap-log` flag either.
        assert!(!calls[0].args.iter().any(|a| a == "--wrap-log"));
        let cwd_pos = calls[0]
            .args
            .iter()
            .position(|a| a == "--cwd")
            .expect("--cwd missing");
        assert_eq!(calls[0].args[cwd_pos + 1], "/work/alpha");
        let backend_pos = calls[0]
            .args
            .iter()
            .position(|a| a == "--backend")
            .expect("--backend missing");
        assert_eq!(calls[0].args[backend_pos + 1], "psmux");
        assert!(calls[0].args.iter().any(|a| a == "claude"));
        // Tagged with the cwd's directory name, not a session id.
        assert_eq!(
            calls[1].args,
            vec!["select-pane", "-t", "play:%1", "-T", "alpha"]
        );
    }

    #[test]
    fn open_new_session_anchors_the_split_when_given_an_anchor_pane() {
        let runner = MockRunner::default();

        open_new_session(
            Some(Backend::Psmux),
            &PathBuf::from("/work/alpha"),
            runner.clone(),
            Some("%7"),
            None,
        )
        .unwrap();

        let calls = runner.calls();
        let t = calls[0]
            .args
            .iter()
            .position(|a| a == "-t")
            .expect("-t missing");
        assert_eq!(calls[0].args[t + 1], "%7");
    }

    #[test]
    fn open_new_session_title_falls_back_to_the_full_path_without_a_file_name() {
        let runner = MockRunner::default();

        open_new_session(
            Some(Backend::Psmux),
            &PathBuf::from("/"),
            runner.clone(),
            None,
            None,
        )
        .unwrap();

        let calls = runner.calls();
        assert_eq!(
            calls[1].args,
            vec!["select-pane", "-t", "play:%1", "-T", "/"]
        );
    }

    #[test]
    fn open_new_session_with_no_backend_reports_no_backend_detected() {
        let runner = MockRunner::default();

        let outcome = open_new_session(
            None,
            &PathBuf::from("/work/alpha"),
            runner.clone(),
            None,
            None,
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::NoBackendDetected);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn open_new_session_passes_wrap_log_through_to_the_wrap_argv() {
        let runner = MockRunner::default();

        open_new_session(
            Some(Backend::Psmux),
            &PathBuf::from("/work/alpha"),
            runner.clone(),
            None,
            Some("C:/logs/wrap.log"),
        )
        .unwrap();

        let calls = runner.calls();
        let pos = calls[0]
            .args
            .iter()
            .position(|a| a == "--wrap-log")
            .expect("--wrap-log missing");
        assert_eq!(calls[0].args[pos + 1], "C:/logs/wrap.log");
    }

    // --- encode_target / decode_handle -----------------------------------

    #[test]
    fn encode_decode_tmux_target_roundtrips_the_session_qualified_form() {
        let handle = SessionHandle::Tmux {
            session: "play".to_string(),
            window_id: "@3".to_string(),
            pane_id: "%8".to_string(),
        };

        let target = encode_target(&handle);

        assert_eq!(target, "play:@3:%8");
        assert_eq!(decode_handle(Backend::Psmux, &target), Some(handle));
    }

    #[test]
    fn decode_handle_rejects_a_pre_session_qualification_two_part_target() {
        // A pane record written before psmux's non-unique-id finding
        // (docs/notes/psmux-spike.md, 2026-07-20) encodes the old
        // `<window_id>:<pane_id>` form. It can no longer be resolved to a
        // specific session, so it must decode to `None`, not be
        // misinterpreted as `<session>:<pane_id>` with a missing pane.
        assert_eq!(decode_handle(Backend::Psmux, "@3:%8"), None);
    }

    #[test]
    fn encode_target_windows_terminal_is_empty() {
        assert_eq!(encode_target(&SessionHandle::WindowsTerminal), "");
    }

    #[test]
    fn decode_handle_windows_terminal_ignores_the_target_string() {
        assert_eq!(
            decode_handle(Backend::WindowsTerminal, ""),
            Some(SessionHandle::WindowsTerminal)
        );
    }

    // --- resolve_own_anchor / resolve_own_session -------------------------

    #[test]
    fn resolve_own_anchor_queries_the_session_name_and_qualifies_the_pane() {
        let runner = MockRunner::with_target_responses([CommandOutput::success("play\n")]);

        let anchor = resolve_own_anchor(Some(Backend::Psmux), &runner, Some("%2"));

        assert_eq!(anchor.as_deref(), Some("play:%2"));
        assert_eq!(
            runner.calls(),
            vec![CommandSpec::new(
                "psmux",
                ["display-message", "-p", "#{session_name}"].map(str::to_string)
            )]
        );
    }

    #[test]
    fn resolve_own_anchor_falls_back_to_the_bare_pane_when_session_lookup_fails() {
        let runner = MockRunner::with_target_responses([CommandOutput::failure(
            Some(1),
            "no server running".to_string(),
        )]);

        let anchor = resolve_own_anchor(Some(Backend::Psmux), &runner, Some("%2"));

        assert_eq!(anchor.as_deref(), Some("%2"));
    }

    #[test]
    fn resolve_own_anchor_is_none_without_a_tmux_pane() {
        let runner = MockRunner::with_target_responses([CommandOutput::success("play\n")]);

        assert_eq!(
            resolve_own_anchor(Some(Backend::Psmux), &runner, None),
            None
        );
        // Never shells out when there's no pane to qualify in the first place.
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn resolve_own_anchor_is_none_for_a_non_psmux_backend_even_with_tmux_pane_set() {
        // Mirrors `crate::wrap::resolve_own_pane`'s reasoning: banto could be
        // running inside an unrelated psmux session (so `$TMUX_PANE` is set)
        // while still configured to open new sessions via Windows Terminal.
        let runner = MockRunner::with_target_responses([CommandOutput::success("play\n")]);

        assert_eq!(
            resolve_own_anchor(Some(Backend::WindowsTerminal), &runner, Some("%2")),
            None
        );
        assert_eq!(resolve_own_anchor(None, &runner, Some("%2")), None);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn wrap_argv_uses_the_given_exe_path_when_available() {
        assert_eq!(
            wrap_argv(Some("C:/dev/banto.exe"), "sess-1"),
            [
                "C:/dev/banto.exe",
                "_wrap",
                "--session",
                "sess-1",
                "--",
                "claude",
                "--resume",
                "sess-1",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn wrap_argv_falls_back_to_the_bare_name_without_an_exe_path() {
        assert_eq!(wrap_argv(None, "sess-1")[0], "banto");
    }

    #[test]
    fn new_session_wrap_argv_uses_the_given_exe_path_when_available() {
        assert_eq!(
            new_session_wrap_argv(
                Some("C:/dev/banto.exe"),
                Path::new("/work/alpha"),
                Backend::Psmux,
                None
            ),
            [
                "C:/dev/banto.exe",
                "_wrap",
                "--new-session",
                "--cwd",
                "/work/alpha",
                "--backend",
                "psmux",
                "--",
                "claude",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn new_session_wrap_argv_falls_back_to_the_bare_name_without_an_exe_path() {
        assert_eq!(
            new_session_wrap_argv(None, Path::new("/work/alpha"), Backend::Psmux, None)[0],
            "banto"
        );
    }

    #[test]
    fn new_session_wrap_argv_passes_the_windows_terminal_backend_key() {
        let argv = new_session_wrap_argv(
            None,
            Path::new("/work/alpha"),
            Backend::WindowsTerminal,
            None,
        );
        let pos = argv
            .iter()
            .position(|a| a == "--backend")
            .expect("--backend missing");
        assert_eq!(argv[pos + 1], "windows-terminal");
    }

    #[test]
    fn new_session_wrap_argv_appends_wrap_log_before_the_trailing_claude_argv() {
        let argv = new_session_wrap_argv(
            None,
            Path::new("/work/alpha"),
            Backend::Psmux,
            Some("C:/logs/wrap.log"),
        );
        let pos = argv
            .iter()
            .position(|a| a == "--wrap-log")
            .expect("--wrap-log missing");
        assert_eq!(argv[pos + 1], "C:/logs/wrap.log");
        // Still ends with the separator and the wrapped command.
        assert_eq!(&argv[pos + 2..], ["--", "claude"]);
    }

    #[test]
    fn new_session_wrap_argv_omits_wrap_log_when_not_given() {
        let argv = new_session_wrap_argv(None, Path::new("/work/alpha"), Backend::Psmux, None);
        assert!(!argv.iter().any(|a| a == "--wrap-log"));
    }

    #[test]
    fn resolve_backend_forces_configured_backend_regardless_of_env() {
        assert_eq!(
            resolve_backend(OpenerMode::Psmux, |_| Some("set".to_string())),
            Some(Backend::Psmux)
        );
        assert_eq!(
            resolve_backend(OpenerMode::WindowsTerminal, |_| None),
            Some(Backend::WindowsTerminal)
        );
    }

    #[test]
    fn resolve_backend_auto_detects_from_env() {
        assert_eq!(
            resolve_backend(OpenerMode::Auto, |k| (k == "TMUX").then(|| "1".to_string())),
            Some(Backend::Psmux)
        );
        assert_eq!(resolve_backend(OpenerMode::Auto, |_| None), None);
    }
}
