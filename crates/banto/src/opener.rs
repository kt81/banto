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

use std::path::PathBuf;
use std::time::SystemTime;

use banto_core::config::OpenerMode;
use banto_core::model::SessionId;
use banto_core::opener::{self, Backend, CommandRunner, OpenError, ResumeCommand, SessionHandle};
use banto_core::status::ProcessProbe;
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

/// Open or focus `session`, enforcing the no-double-resume invariant.
///
/// `backend` is the resolved opener backend for *new* opens (see
/// [`resolve_backend`]); an existing pane is always focused through whichever
/// backend it was originally opened with, regardless of the current default.
pub fn open_session<R: CommandRunner + Clone + 'static>(
    store: &Store,
    probe: &dyn ProcessProbe,
    backend: Option<Backend>,
    session: &SessionToOpen,
    runner: R,
) -> Result<OpenOutcome, SessionOpenError> {
    let id = SessionId(session.id.clone());

    if let Some(record) = store.get_pane(&id)? {
        // No PID yet means the wrapper is still starting up; assume alive
        // rather than risk a double resume.
        let alive = record.pid.is_none_or(|pid| probe.is_alive(pid));
        if alive {
            match focus_existing(store, &record, runner.clone())? {
                FocusResult::Outcome(outcome) => return Ok(outcome),
                // The backend CLI ran but reported the pane doesn't exist
                // any more (already cleaned up); fall through to open fresh.
                FocusResult::Stale => {}
            }
        } else {
            // The wrapper is gone but never cleaned up (e.g. it crashed).
            store.remove_pane(&id)?;
        }
    }

    open_fresh(store, backend, session, runner)
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
    let handle = opener::opener_for(backend, runner).open(&cmd)?;

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

/// Stable string stored in [`PaneRecord::backend`] (kebab-case, independent
/// of `Backend`'s human-readable `Display`).
fn backend_key(backend: Backend) -> &'static str {
    match backend {
        Backend::Psmux => "psmux",
        Backend::WindowsTerminal => "windows-terminal",
    }
}

fn parse_backend_key(key: &str) -> Option<Backend> {
    match key {
        "psmux" => Some(Backend::Psmux),
        "windows-terminal" => Some(Backend::WindowsTerminal),
        _ => None,
    }
}

/// Encode a [`SessionHandle`] into [`PaneRecord::target`].
fn encode_target(handle: &SessionHandle) -> String {
    match handle {
        SessionHandle::Tmux { window_id, pane_id } => format!("{window_id}:{pane_id}"),
        SessionHandle::WindowsTerminal => String::new(),
    }
}

/// Inverse of [`encode_target`].
fn decode_handle(backend: Backend, target: &str) -> Option<SessionHandle> {
    match backend {
        Backend::Psmux => {
            let (window_id, pane_id) = target.split_once(':')?;
            Some(SessionHandle::Tmux {
                window_id: window_id.to_string(),
                pane_id: pane_id.to_string(),
            })
        }
        Backend::WindowsTerminal => Some(SessionHandle::WindowsTerminal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::opener::{CommandOutput, CommandSpec};
    use std::cell::{Cell, RefCell};
    use std::collections::HashSet;
    use std::rc::Rc;

    /// Records every command and always reports success with a fixed
    /// `create` output for commands that need to parse one (psmux's
    /// `-P -F` create format) — unless configured via
    /// [`MockRunner::failing_select_window`] to simulate the backend CLI
    /// reporting the pane no longer exists. Cheaply `Clone`-able (shared
    /// call log) so a clone can be moved into `open_session` while the
    /// original is kept around to inspect the calls afterward.
    #[derive(Clone, Default)]
    struct MockRunner {
        calls: Rc<RefCell<Vec<CommandSpec>>>,
        fail_select_window: Rc<Cell<bool>>,
    }

    impl MockRunner {
        fn calls(&self) -> Vec<CommandSpec> {
            self.calls.borrow().clone()
        }

        /// A runner whose `select-window` calls fail, simulating psmux
        /// reporting the pane is gone (e.g. it was closed out of band).
        fn failing_select_window() -> Self {
            let runner = Self::default();
            runner.fail_select_window.set(true);
            runner
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, OpenError> {
            self.calls.borrow_mut().push(spec.clone());
            if self.fail_select_window.get()
                && spec.args.first().map(String::as_str) == Some("select-window")
            {
                return Ok(CommandOutput::failure(
                    Some(1),
                    "no such window".to_string(),
                ));
            }
            Ok(CommandOutput::success("@1:%1\n"))
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
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let record = store
            .get_pane(&SessionId("sess-1".to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(record.backend, "psmux");
        assert_eq!(record.target, "@1:%1");
        assert_eq!(record.pid, None);
    }

    #[test]
    fn no_backend_detected_opens_nothing() {
        let store = Store::open_in_memory().unwrap();
        let probe = MockProbe::with_alive(&[]);
        let runner = MockRunner::default();

        let outcome = open_session(&store, &probe, None, &session(), runner.clone()).unwrap();

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
                target: "@3:%8".to_string(),
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
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Focused);
        let calls = runner.calls();
        assert_eq!(calls.len(), 2); // select-window + select-pane
        assert_eq!(calls[0].args, vec!["select-window", "-t", "@3"]);
        assert_eq!(calls[1].args, vec!["select-pane", "-t", "%8"]);
    }

    #[test]
    fn missing_pid_is_treated_as_alive_to_avoid_double_resume() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "psmux".to_string(),
                target: "@3:%8".to_string(),
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
                target: "@3:%8".to_string(),
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
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let record = store
            .get_pane(&SessionId("sess-1".to_string()))
            .unwrap()
            .unwrap();
        // Replaced with the freshly opened pane, not the stale one.
        assert_eq!(record.target, "@1:%1");
    }

    #[test]
    fn focus_command_error_is_treated_as_stale_and_opens_fresh() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "psmux".to_string(),
                target: "@3:%8".to_string(),
                pid: Some(100),
                opened_at: SystemTime::now(),
            })
            .unwrap();
        // Pid alive, so a normal focus would be attempted, but the backend
        // reports the pane itself is gone (e.g. closed out of band).
        let probe = MockProbe::with_alive(&[100]);
        let runner = MockRunner::failing_select_window();

        let outcome = open_session(
            &store,
            &probe,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let record = store
            .get_pane(&SessionId("sess-1".to_string()))
            .unwrap()
            .unwrap();
        // Replaced with the freshly opened pane, not the stale one.
        assert_eq!(record.target, "@1:%1");
        assert_ne!(record.target, "@3:%8");
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
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::AlreadyOpenCannotFocus);
        // Never attempted to open a second tab.
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
