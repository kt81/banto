//! `banto _wrap`: supervises the resumed session's process.
//!
//! This runs as the direct child spawned by the opener backend (psmux /
//! Windows Terminal). It registers its own PID against the session's pane
//! record so [`crate::opener::open_session`] can later tell a live pane from
//! a stale one by PID liveness alone (no need to query the terminal backend),
//! then removes the record once the wrapped process exits. Per
//! docs/REQUIREMENTS.md "Opener spec", this wrapper is what makes
//! double-resume prevention possible at all, since `wt.exe` detaches
//! immediately and leaves nothing else to track.

use anyhow::{Context, Result};

use banto_core::model::SessionId;
use banto_core::store::Store;

use crate::process::ProcessRunner;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::mock::MockProcessRunner;
    use banto_core::store::PaneRecord;
    use std::time::SystemTime;

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
}
