//! `banto _wrap`: supervises the resumed/new session's process.
//!
//! This runs as the direct child spawned by the opener backend (psmux /
//! Windows Terminal), in one of two modes:
//! - **Resume** ([`run`]): the session id is already known, and the opener
//!   already wrote a pane record before this process started (it had the
//!   handle from creating the pane) — this just attaches its own PID to
//!   that record so [`crate::opener::open_session`] can later tell a live
//!   pane from a stale one by PID liveness alone (no need to query the
//!   terminal backend), then removes the record once the wrapped process
//!   exits.
//! - **New session** ([`run_new_session`]): there is no known session id up
//!   front (`n`-launched, plain `claude`) and therefore no pre-existing pane
//!   record to attach to — this process discovers both where it landed
//!   ([`resolve_own_pane`]) and which session id Claude assigns while the
//!   child is still running ([`track_new_session`]), and writes the pane
//!   record itself.
//!
//! Per docs/REQUIREMENTS.md "Opener spec", this wrapper is what makes
//! double-resume prevention possible at all, since `wt.exe` detaches
//! immediately and leaves nothing else to track.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use banto_core::model::SessionId;
use banto_core::opener::{Backend, CommandRunner, CommandSpec, SessionHandle};
use banto_core::provider::claude_code::ClaudeCodeProvider;
use banto_core::store::{PaneRecord, Store};

use crate::process::{ProcessRunner, SpawnedProcess};

/// Register this process's PID, run `argv` to completion, then clean up the
/// pane record. Returns the child's exit code (or `1` if it could not be
/// determined, e.g. terminated by a signal).
pub fn run(
    store: &Store,
    session: &str,
    argv: &[String],
    runner: &dyn ProcessRunner,
) -> Result<i32> {
    let id = SessionId(session.to_string());

    register_pid(store, &id);

    let code = runner
        .run(argv)
        .with_context(|| format!("failed to run wrapped command: {argv:?}"))?;

    // Best-effort cleanup: a store failure here must not mask the child's
    // own exit code.
    let _ = store.remove_pane(&id);

    Ok(code.unwrap_or(1))
}

/// Attach this process's PID to the pane record [`crate::opener::open_session`]
/// already wrote, so liveness can be checked without querying the terminal
/// backend. A missing or unreadable record is tolerated: the session simply
/// won't be double-resume-protected until the next successful open.
fn register_pid(store: &Store, id: &SessionId) {
    if let Ok(Some(mut record)) = store.get_pane(id) {
        record.pid = Some(std::process::id());
        let _ = store.set_pane(&record);
    }
}

/// How long [`track_new_session`] waits between polls (both for the
/// child's exit and, until found, the session id Claude assigns). Short
/// enough that a freshly-launched session becomes focusable well within
/// normal human reaction time, without busy-looping.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Launch a brand-new (non-resumed) session, tracking it so it can later be
/// focused instead of double-resumed. Unlike [`run`], there is no known
/// session id up front (`argv` is plain `claude`, no `--resume <id>`), so
/// this process discovers both where it itself landed
/// ([`resolve_own_pane`]) and which session id Claude assigns to `cwd`
/// ([`ClaudeCodeProvider::find_new_session`]) while the child is still
/// running ([`track_new_session`]), then writes the pane record itself —
/// there is no pre-existing one from an `open_*` call to attach to, since
/// the opener that spawned this process had no session id to key one
/// against either.
///
/// Skips tracking entirely when [`resolve_own_pane`] finds no focusable
/// pane (e.g. Windows Terminal, which exposes no handle to bring anything
/// back to the front with): there is nothing useful to record, so the
/// child just runs to completion directly, exactly like a plain launch.
pub fn run_new_session(
    store: &Store,
    cwd: &Path,
    argv: &[String],
    process_runner: &dyn ProcessRunner,
    command_runner: &dyn CommandRunner,
    provider: &ClaudeCodeProvider,
    env: impl Fn(&str) -> Option<String>,
) -> Result<i32> {
    let Some((backend, target)) = resolve_own_pane(env, command_runner) else {
        let code = process_runner
            .run(argv)
            .with_context(|| format!("failed to run new session: {argv:?}"))?;
        return Ok(code.unwrap_or(1));
    };

    let since = SystemTime::now();
    let mut spawned = process_runner
        .spawn(argv)
        .with_context(|| format!("failed to spawn new session: {argv:?}"))?;

    let mut recorded: Option<SessionId> = None;
    let code = track_new_session(
        spawned.as_mut(),
        || provider.find_new_session(cwd, since),
        |id| {
            let record = new_session_pane_record(id.clone(), backend.clone(), target.clone());
            let _ = store.set_pane(&record);
            recorded = Some(id);
        },
        || std::thread::sleep(POLL_INTERVAL),
    )?;

    // Best-effort cleanup, same rationale as `run`: a store failure here
    // must not mask the child's own exit code.
    if let Some(id) = recorded {
        let _ = store.remove_pane(&id);
    }

    Ok(code.unwrap_or(1))
}

/// Build the [`PaneRecord`] for a newly-discovered session, tagged with
/// this process's own pid — mirroring [`register_pid`]'s resume-mode
/// convention, since `_wrap` is what stays alive for the session's whole
/// lifetime either way.
fn new_session_pane_record(id: SessionId, backend: String, target: String) -> PaneRecord {
    PaneRecord {
        session_id: id,
        backend,
        target,
        pid: Some(std::process::id()),
        opened_at: SystemTime::now(),
    }
}

/// Resolve this process's own pane as `(backend_key, target)` for a
/// [`PaneRecord`], or `None` if it isn't running inside a backend whose
/// pane can later be focused.
///
/// Reads `$TMUX_PANE` (set by psmux for every pane) itself, via the
/// injected `env` lookup, rather than being told via a CLI flag: the resume
/// flow's pane record is written by the *opener* before this process ever
/// starts (it already has the handle from creating the pane), but for a
/// freshly-launched new session there is no pre-existing record — this
/// process is the only one that can discover where it landed. Absence of
/// `$TMUX_PANE` means Windows Terminal (or a bare terminal): there is no
/// handle to record in that case (see [`run_new_session`]).
///
/// Passes `-t <pane_id>` explicitly rather than relying on
/// `display-message`'s default target, which resolves to the *client's*
/// currently active pane, not necessarily this one — the same ambiguity
/// `TmuxOpener::with_anchor_pane` documents for `split-window`.
fn resolve_own_pane(
    env: impl Fn(&str) -> Option<String>,
    runner: &dyn CommandRunner,
) -> Option<(String, String)> {
    let pane_id = env("TMUX_PANE")?;
    let output = runner
        .run(&CommandSpec::new(
            "psmux",
            [
                "display-message",
                "-p",
                "-t",
                pane_id.as_str(),
                "-F",
                "#{window_id}",
            ]
            .map(str::to_string),
        ))
        .ok()?;
    if !output.success {
        return None;
    }
    let window_id = output.stdout.trim();
    if window_id.is_empty() {
        return None;
    }
    let handle = SessionHandle::Tmux {
        window_id: window_id.to_string(),
        pane_id,
    };
    Some((
        crate::opener::backend_key(Backend::Psmux).to_string(),
        crate::opener::encode_target(&handle),
    ))
}

/// Poll `spawned` until it exits, meanwhile discovering the session id it
/// creates for the launch (via `find_new_session`, called every poll until
/// it succeeds once) and recording it (via `record_pane`, called exactly
/// once, the first time discovery succeeds) so the launched session can
/// later be focused instead of double-resumed. `sleep` paces each
/// iteration — a real caller sleeps for [`POLL_INTERVAL`]; tests inject a
/// no-op so the loop runs at closure speed, driven purely by the mocks'
/// return sequences.
fn track_new_session(
    spawned: &mut dyn SpawnedProcess,
    mut find_new_session: impl FnMut() -> Option<SessionId>,
    mut record_pane: impl FnMut(SessionId),
    mut sleep: impl FnMut(),
) -> Result<Option<i32>> {
    let mut found = false;
    loop {
        if let Some(code) = spawned.try_wait()? {
            return Ok(code);
        }
        if !found && let Some(id) = find_new_session() {
            record_pane(id);
            found = true;
        }
        sleep();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::mock::{MockProcessRunner, MockSpawnedProcess};
    use banto_core::opener::{CommandOutput, OpenError};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::fs;
    use tempfile::TempDir;

    fn pane(session: &str) -> PaneRecord {
        PaneRecord {
            session_id: SessionId(session.to_string()),
            backend: "psmux".to_string(),
            target: "@1:%1".to_string(),
            pid: None,
            opened_at: SystemTime::now(),
        }
    }

    #[test]
    fn registers_pid_runs_child_and_cleans_up_on_exit() {
        let store = Store::open_in_memory().unwrap();
        store.set_pane(&pane("s1")).unwrap();
        let runner = MockProcessRunner::new(Some(0));
        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "s1".to_string(),
        ];

        let code = run(&store, "s1", &argv, &runner).unwrap();

        assert_eq!(code, 0);
        assert_eq!(runner.calls(), vec![argv]);
        // Cleaned up: the pane record is gone once the child exits.
        assert_eq!(store.get_pane(&SessionId("s1".to_string())).unwrap(), None);
    }

    #[test]
    fn register_pid_updates_existing_record_with_own_pid() {
        let store = Store::open_in_memory().unwrap();
        store.set_pane(&pane("s1")).unwrap();
        let id = SessionId("s1".to_string());

        register_pid(&store, &id);

        let record = store.get_pane(&id).unwrap().unwrap();
        assert_eq!(record.pid, Some(std::process::id()));
        assert_eq!(record.backend, "psmux");
        assert_eq!(record.target, "@1:%1");
    }

    #[test]
    fn register_pid_is_a_noop_when_no_record_exists() {
        let store = Store::open_in_memory().unwrap();
        let id = SessionId("missing".to_string());

        register_pid(&store, &id); // must not panic

        assert_eq!(store.get_pane(&id).unwrap(), None);
    }

    #[test]
    fn missing_pane_record_does_not_prevent_running_or_error() {
        let store = Store::open_in_memory().unwrap();
        let runner = MockProcessRunner::new(Some(7));

        let code = run(&store, "unknown", &["claude".to_string()], &runner).unwrap();

        assert_eq!(code, 7);
    }

    #[test]
    fn signal_termination_maps_to_exit_code_one() {
        let store = Store::open_in_memory().unwrap();
        let runner = MockProcessRunner::new(None);

        let code = run(&store, "s1", &["claude".to_string()], &runner).unwrap();

        assert_eq!(code, 1);
    }

    /// A [`CommandRunner`] that records every spec and replays queued
    /// outputs in order (falling back to an empty success once exhausted).
    /// Mirrors `banto_core::opener::command::mock::MockRunner`, which is
    /// private to that crate.
    #[derive(Default)]
    struct MockCommandRunner {
        responses: RefCell<VecDeque<CommandOutput>>,
        calls: RefCell<Vec<CommandSpec>>,
    }

    impl MockCommandRunner {
        fn with_responses(responses: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                calls: RefCell::default(),
            }
        }

        fn calls(&self) -> Vec<CommandSpec> {
            self.calls.borrow().clone()
        }
    }

    impl CommandRunner for MockCommandRunner {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, OpenError> {
            self.calls.borrow_mut().push(spec.clone());
            Ok(self
                .responses
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| CommandOutput::success("")))
        }
    }

    // --- resolve_own_pane ---------------------------------------------

    #[test]
    fn resolve_own_pane_is_none_without_tmux_pane_env() {
        let runner = MockCommandRunner::default();

        assert_eq!(resolve_own_pane(|_| None, &runner), None);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn resolve_own_pane_resolves_backend_and_target_via_display_message() {
        let runner = MockCommandRunner::with_responses([CommandOutput::success("@3\n")]);
        let env = |key: &str| (key == "TMUX_PANE").then(|| "%8".to_string());

        let result = resolve_own_pane(env, &runner);

        assert_eq!(result, Some(("psmux".to_string(), "@3:%8".to_string())));
        assert_eq!(
            runner.calls(),
            vec![CommandSpec::new(
                "psmux",
                ["display-message", "-p", "-t", "%8", "-F", "#{window_id}"].map(str::to_string)
            )]
        );
    }

    #[test]
    fn resolve_own_pane_is_none_when_display_message_fails() {
        let runner = MockCommandRunner::with_responses([CommandOutput::failure(
            Some(1),
            "no server running".to_string(),
        )]);
        let env = |key: &str| (key == "TMUX_PANE").then(|| "%8".to_string());

        assert_eq!(resolve_own_pane(env, &runner), None);
    }

    #[test]
    fn resolve_own_pane_is_none_when_display_message_returns_empty_output() {
        let runner = MockCommandRunner::with_responses([CommandOutput::success("  \n")]);
        let env = |key: &str| (key == "TMUX_PANE").then(|| "%8".to_string());

        assert_eq!(resolve_own_pane(env, &runner), None);
    }

    // --- new_session_pane_record ----------------------------------------

    #[test]
    fn new_session_pane_record_carries_the_id_backend_target_and_own_pid() {
        let record = new_session_pane_record(
            SessionId("new-id".to_string()),
            "psmux".to_string(),
            "@3:%8".to_string(),
        );

        assert_eq!(record.session_id, SessionId("new-id".to_string()));
        assert_eq!(record.backend, "psmux");
        assert_eq!(record.target, "@3:%8");
        assert_eq!(record.pid, Some(std::process::id()));
    }

    // --- track_new_session -----------------------------------------------

    #[test]
    fn track_new_session_returns_immediately_if_already_exited() {
        let mut spawned = MockSpawnedProcess::new(0, Some(5));
        let find_calls = Cell::new(0);
        let recorded: RefCell<Vec<SessionId>> = RefCell::default();

        let code = track_new_session(
            &mut spawned,
            || {
                find_calls.set(find_calls.get() + 1);
                None
            },
            |id| recorded.borrow_mut().push(id),
            || {},
        )
        .unwrap();

        assert_eq!(code, Some(5));
        assert_eq!(find_calls.get(), 0);
        assert!(recorded.borrow().is_empty());
    }

    #[test]
    fn track_new_session_finds_and_records_exactly_once_then_waits_for_exit() {
        let mut spawned = MockSpawnedProcess::new(3, Some(0));
        let find_calls = Cell::new(0);
        let recorded: RefCell<Vec<SessionId>> = RefCell::default();

        let code = track_new_session(
            &mut spawned,
            || {
                find_calls.set(find_calls.get() + 1);
                (find_calls.get() >= 2).then(|| SessionId("new-id".to_string()))
            },
            |id| recorded.borrow_mut().push(id),
            || {},
        )
        .unwrap();

        assert_eq!(code, Some(0));
        assert_eq!(
            recorded.borrow().as_slice(),
            [SessionId("new-id".to_string())]
        );
        // Found on the 2nd poll; must not keep searching afterward even
        // though the process stays "running" for one more poll after that.
        assert_eq!(find_calls.get(), 2);
    }

    #[test]
    fn track_new_session_never_records_when_the_session_is_never_found_before_exit() {
        let mut spawned = MockSpawnedProcess::new(2, Some(1));
        let recorded: RefCell<Vec<SessionId>> = RefCell::default();

        let code = track_new_session(
            &mut spawned,
            || None,
            |id| recorded.borrow_mut().push(id),
            || {},
        )
        .unwrap();

        assert_eq!(code, Some(1));
        assert!(recorded.borrow().is_empty());
    }

    #[test]
    fn track_new_session_propagates_a_signal_termination_as_none_exit_code() {
        let mut spawned = MockSpawnedProcess::new(0, None);

        let code = track_new_session(&mut spawned, || None, |_| {}, || {}).unwrap();

        assert_eq!(code, None);
    }

    #[test]
    fn track_new_session_paces_one_sleep_per_still_running_poll() {
        let mut spawned = MockSpawnedProcess::new(3, Some(0));
        let sleep_calls = Cell::new(0);

        track_new_session(
            &mut spawned,
            || None,
            |_| {},
            || sleep_calls.set(sleep_calls.get() + 1),
        )
        .unwrap();

        // 3 "still running" polls, each followed by one sleep; the 4th
        // poll (which reports exit) returns immediately without sleeping.
        assert_eq!(sleep_calls.get(), 3);
    }

    // --- run_new_session --------------------------------------------------

    #[test]
    fn run_new_session_skips_tracking_and_runs_directly_without_a_focusable_pane() {
        // No `TMUX_PANE`: e.g. Windows Terminal, which exposes no handle to
        // record anything useful against.
        let claude_home = TempDir::new().unwrap();
        let provider = ClaudeCodeProvider::new(claude_home.path().to_path_buf());
        let store = Store::open_in_memory().unwrap();
        let process_runner = MockProcessRunner::new(Some(0));
        let command_runner = MockCommandRunner::default();

        let code = run_new_session(
            &store,
            Path::new("/work/proj"),
            &["claude".to_string()],
            &process_runner,
            &command_runner,
            &provider,
            |_| None,
        )
        .unwrap();

        assert_eq!(code, 0);
        // The plain, blocking path was used, not spawn+tracking.
        assert_eq!(process_runner.calls(), vec![vec!["claude".to_string()]]);
        assert!(process_runner.spawn_calls().is_empty());
        assert!(command_runner.calls().is_empty());
    }

    #[test]
    fn run_new_session_tracks_via_spawn_when_a_focusable_pane_is_resolved() {
        let claude_home = TempDir::new().unwrap();
        let provider = ClaudeCodeProvider::new(claude_home.path().to_path_buf());
        let store = Store::open_in_memory().unwrap();
        // Exits on the very first poll (0 still-running polls): no session
        // file exists to be found either, so nothing should be recorded.
        let process_runner = MockProcessRunner::new_spawning(0, Some(0));
        let command_runner = MockCommandRunner::with_responses([CommandOutput::success("@3\n")]);
        let env = |key: &str| (key == "TMUX_PANE").then(|| "%8".to_string());

        let code = run_new_session(
            &store,
            Path::new("/work/proj"),
            &["claude".to_string()],
            &process_runner,
            &command_runner,
            &provider,
            env,
        )
        .unwrap();

        assert_eq!(code, 0);
        // The spawn+tracking path was used, not the plain blocking one.
        assert_eq!(
            process_runner.spawn_calls(),
            vec![vec!["claude".to_string()]]
        );
        assert!(process_runner.calls().is_empty());
        // Resolved its own pane via display-message before tracking.
        assert_eq!(command_runner.calls().len(), 1);
        // Never found a match (no session file exists), so nothing was
        // ever recorded to begin with.
        assert!(store.list_panes().unwrap().is_empty());
    }

    #[test]
    fn run_new_session_discovers_and_removes_the_pane_record_by_the_time_it_returns() {
        // The session file exists with a mtime comfortably in the future,
        // so it's always found regardless of exactly when `since` gets
        // captured inside `run_new_session`.
        let claude_home = TempDir::new().unwrap();
        let project_dir = claude_home.path().join("projects").join("proj");
        fs::create_dir_all(&project_dir).unwrap();
        let session_file = project_dir.join("new-id.jsonl");
        fs::write(
            &session_file,
            "{\"type\":\"user\",\"cwd\":\"/work/proj\",\"message\":{\"content\":\"hi\"}}\n",
        )
        .unwrap();
        let future = SystemTime::now() + Duration::from_secs(3600);
        fs::OpenOptions::new()
            .write(true)
            .open(&session_file)
            .unwrap()
            .set_modified(future)
            .unwrap();

        let provider = ClaudeCodeProvider::new(claude_home.path().to_path_buf());
        let store = Store::open_in_memory().unwrap();
        // Runs for one "still running" poll (long enough for discovery to
        // happen before exit) then exits.
        let process_runner = MockProcessRunner::new_spawning(1, Some(0));
        let command_runner = MockCommandRunner::with_responses([CommandOutput::success("@3\n")]);
        let env = |key: &str| (key == "TMUX_PANE").then(|| "%8".to_string());

        let code = run_new_session(
            &store,
            Path::new("/work/proj"),
            &["claude".to_string()],
            &process_runner,
            &command_runner,
            &provider,
            env,
        )
        .unwrap();

        assert_eq!(code, 0);
        // Cleaned up once the child exits: the exact discovery/record
        // sequence is covered directly by `track_new_session`'s own tests;
        // this only proves the whole thing wires together without leaving
        // a stale record behind.
        assert_eq!(
            store.get_pane(&SessionId("new-id".to_string())).unwrap(),
            None
        );
    }
}
