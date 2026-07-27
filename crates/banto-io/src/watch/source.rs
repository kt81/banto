//! The real filesystem watcher, isolated behind [`ChangeSource`] so callers
//! (and their tests) depend on the trait rather than on `notify` directly.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::SystemTime;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use super::{RawChange, WatchRoot};
use crate::claude_home::ClaudeHome;
use crate::codex_home::CodexHome;

/// Errors setting up the real filesystem watcher.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// `notify` failed to create or configure the underlying OS watcher.
    #[error("failed to start filesystem watcher: {0}")]
    Notify(#[from] notify::Error),
}

/// A source of raw filesystem changes under the watched roots.
///
/// Implemented for real by [`NotifyChangeSource`]; depending on this trait
/// (rather than on `notify` types directly) lets callers substitute a fake
/// source in tests without touching a real filesystem watcher.
pub trait ChangeSource {
    /// Drain every change observed since the last call, without blocking.
    fn drain(&self) -> Vec<RawChange>;
}

/// Watches `<claude_home>/projects/` (recursively), `<claude_home>/sessions/`
/// (flat), and — when a Codex home is given — `<codex_home>/sessions/`
/// (recursively) with `notify`, tagging every observed change with the
/// [`WatchRoot`] it falls under.
pub struct NotifyChangeSource {
    // Kept alive for the lifetime of the source; dropping it stops watching.
    _watcher: RecommendedWatcher,
    changes: Receiver<RawChange>,
}

impl NotifyChangeSource {
    /// Start watching `claude_home`'s `projects/` and `sessions/`
    /// directories, plus `codex_home`'s rollout tree
    /// ([`CodexHome::rollout_dir`]) when one is given.
    ///
    /// Any of the three may not exist yet (a fresh home before its first
    /// session); `notify` errors on watching a missing path, so a missing
    /// root is simply left unwatched, mirroring how
    /// [`crate::provider::claude_code::ClaudeCodeProvider`],
    /// [`crate::provider::codex::CodexProvider`], and
    /// [`crate::status::read_live_sessions`] tolerate the same case. Roots
    /// created after this call is made are not picked up.
    ///
    /// Deliberately never watches any of Codex's sqlite files
    /// (`state_5.sqlite`, `logs_2.sqlite`, ...). Measured against a
    /// synthetic WAL-mode database shaped like the real `state_5.sqlite`: a
    /// write never touches the main file's own mtime (everything lands in
    /// its `-wal`/`-shm` sidecars), and a running Codex session writes
    /// somewhere in its home on every turn — `logs_2.sqlite` on every log
    /// line — so a directory watch there would fire many times per turn
    /// across databases banto doesn't even read, for no discovery benefit
    /// (`session::discover_all` already re-reads `threads` in full on every
    /// fired root, regardless of which one fired). The rollout tree alone
    /// reproduces the fix — a new `rollout-*.jsonl` appearing — at the same
    /// one-event-per-turn rate `projects/` already has for Claude.
    pub fn new(
        claude_home: &ClaudeHome,
        codex_home: Option<&CodexHome>,
    ) -> Result<Self, WatchError> {
        let projects_dir = claude_home.projects_dir();
        let sessions_dir = claude_home.sessions_dir();
        let codex_sessions_dir = codex_home.map(CodexHome::rollout_dir);
        let (tx, rx) = mpsc::channel();

        let watch_projects_dir = projects_dir.clone();
        let watch_sessions_dir = sessions_dir.clone();
        let watch_codex_sessions_dir = codex_sessions_dir.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };
            let now = SystemTime::now();
            for root in classify(
                &event,
                &watch_projects_dir,
                &watch_sessions_dir,
                watch_codex_sessions_dir.as_deref(),
            ) {
                // A send error means the receiving end was dropped (the
                // source is going away); nothing to do about it here.
                let _ = tx.send(RawChange { root, at: now });
            }
        })?;

        if projects_dir.is_dir() {
            watcher.watch(&projects_dir, RecursiveMode::Recursive)?;
        }
        if sessions_dir.is_dir() {
            watcher.watch(&sessions_dir, RecursiveMode::NonRecursive)?;
        }
        if let Some(dir) = &codex_sessions_dir
            && dir.is_dir()
        {
            watcher.watch(dir, RecursiveMode::Recursive)?;
        }

        Ok(Self {
            _watcher: watcher,
            changes: rx,
        })
    }
}

impl ChangeSource for NotifyChangeSource {
    fn drain(&self) -> Vec<RawChange> {
        self.changes.try_iter().collect()
    }
}

/// Which watched roots `event`'s paths fall under (usually zero or one, but
/// an event can name multiple paths, e.g. a rename).
fn classify(
    event: &Event,
    projects_dir: &Path,
    sessions_dir: &Path,
    codex_sessions_dir: Option<&Path>,
) -> Vec<WatchRoot> {
    let mut roots = Vec::new();
    for path in &event.paths {
        let root = if path.starts_with(projects_dir) {
            WatchRoot::Projects
        } else if path.starts_with(sessions_dir) {
            WatchRoot::Sessions
        } else if codex_sessions_dir.is_some_and(|dir| path.starts_with(dir)) {
            WatchRoot::CodexSessions
        } else {
            continue;
        };
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant};

    /// Poll `source` until it reports a change under `want`, or `timeout`
    /// elapses. Real filesystem watchers deliver events asynchronously, so
    /// tests poll instead of asserting on the first `drain()` call.
    fn wait_for(source: &impl ChangeSource, want: WatchRoot, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if source.drain().iter().any(|change| change.root == want) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn detects_a_new_file_under_projects() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("projects")).unwrap();
        let source =
            NotifyChangeSource::new(&ClaudeHome::new(root.path().to_path_buf()), None).unwrap();

        fs::write(root.path().join("projects/new-session.jsonl"), "{}").unwrap();

        assert!(wait_for(
            &source,
            WatchRoot::Projects,
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn detects_a_new_file_under_sessions() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("sessions")).unwrap();
        let source =
            NotifyChangeSource::new(&ClaudeHome::new(root.path().to_path_buf()), None).unwrap();

        fs::write(root.path().join("sessions/1234.json"), "{}").unwrap();

        assert!(wait_for(
            &source,
            WatchRoot::Sessions,
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn detects_a_new_rollout_file_deep_under_the_codex_sessions_tree() {
        // Mirrors the real layout confirmed against a live installed Codex
        // CLI: `sessions/YYYY/MM/DD/rollout-*.jsonl`, none of which exists
        // until the moment a session starts.
        let claude_root = tempfile::tempdir().unwrap();
        let codex_root = tempfile::tempdir().unwrap();
        fs::create_dir_all(codex_root.path().join("sessions")).unwrap();
        let source = NotifyChangeSource::new(
            &ClaudeHome::new(claude_root.path().to_path_buf()),
            Some(&CodexHome::new(codex_root.path().to_path_buf())),
        )
        .unwrap();

        let day_dir = codex_root.path().join("sessions/2026/07/27");
        fs::create_dir_all(&day_dir).unwrap();
        fs::write(day_dir.join("rollout-thread-1.jsonl"), "{}").unwrap();

        assert!(wait_for(
            &source,
            WatchRoot::CodexSessions,
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn a_change_under_claude_home_is_not_misclassified_as_codex() {
        let claude_root = tempfile::tempdir().unwrap();
        let codex_root = tempfile::tempdir().unwrap();
        fs::create_dir_all(claude_root.path().join("projects")).unwrap();
        fs::create_dir_all(codex_root.path().join("sessions")).unwrap();
        let source = NotifyChangeSource::new(
            &ClaudeHome::new(claude_root.path().to_path_buf()),
            Some(&CodexHome::new(codex_root.path().to_path_buf())),
        )
        .unwrap();

        fs::write(claude_root.path().join("projects/new-session.jsonl"), "{}").unwrap();

        assert!(wait_for(
            &source,
            WatchRoot::Projects,
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn missing_roots_construct_successfully_and_never_report_changes() {
        let claude_root = tempfile::tempdir().unwrap();
        let codex_root = tempfile::tempdir().unwrap();

        let source = NotifyChangeSource::new(
            &ClaudeHome::new(claude_root.path().to_path_buf()),
            Some(&CodexHome::new(codex_root.path().to_path_buf())),
        )
        .unwrap();

        std::thread::sleep(Duration::from_millis(50));
        assert!(source.drain().is_empty());
    }

    #[test]
    fn no_codex_home_is_tolerated_same_as_a_missing_claude_root() {
        let claude_root = tempfile::tempdir().unwrap();
        let source =
            NotifyChangeSource::new(&ClaudeHome::new(claude_root.path().to_path_buf()), None)
                .unwrap();

        std::thread::sleep(Duration::from_millis(50));
        assert!(source.drain().is_empty());
    }
}
