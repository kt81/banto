//! The real filesystem watcher, isolated behind [`ChangeSource`] so callers
//! (and their tests) depend on the trait rather than on `notify` directly.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::SystemTime;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use super::{RawChange, WatchRoot};
use crate::claude_home::ClaudeHome;

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

/// Watches `<claude_home>/projects/` (recursively) and
/// `<claude_home>/sessions/` (flat) with `notify`, tagging every observed
/// change with the [`WatchRoot`] it falls under.
pub struct NotifyChangeSource {
    // Kept alive for the lifetime of the source; dropping it stops watching.
    _watcher: RecommendedWatcher,
    changes: Receiver<RawChange>,
}

impl NotifyChangeSource {
    /// Start watching `claude_home`'s `projects/` and `sessions/`
    /// directories.
    ///
    /// Either directory may not exist yet (a fresh Claude home before its
    /// first session); `notify` errors on watching a missing path, so a
    /// missing root is simply left unwatched, mirroring how
    /// [`crate::provider::claude_code::ClaudeCodeProvider`] and
    /// [`crate::status::read_live_sessions`] tolerate the same case. Roots
    /// created after this call is made are not picked up.
    pub fn new(claude_home: &ClaudeHome) -> Result<Self, WatchError> {
        let projects_dir = claude_home.projects_dir();
        let sessions_dir = claude_home.sessions_dir();
        let (tx, rx) = mpsc::channel();

        let watch_projects_dir = projects_dir.clone();
        let watch_sessions_dir = sessions_dir.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };
            let now = SystemTime::now();
            for root in classify(&event, &watch_projects_dir, &watch_sessions_dir) {
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
fn classify(event: &Event, projects_dir: &Path, sessions_dir: &Path) -> Vec<WatchRoot> {
    let mut roots = Vec::new();
    for path in &event.paths {
        let root = if path.starts_with(projects_dir) {
            WatchRoot::Projects
        } else if path.starts_with(sessions_dir) {
            WatchRoot::Sessions
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
        let source = NotifyChangeSource::new(&ClaudeHome::new(root.path().to_path_buf())).unwrap();

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
        let source = NotifyChangeSource::new(&ClaudeHome::new(root.path().to_path_buf())).unwrap();

        fs::write(root.path().join("sessions/1234.json"), "{}").unwrap();

        assert!(wait_for(
            &source,
            WatchRoot::Sessions,
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn missing_roots_construct_successfully_and_never_report_changes() {
        let root = tempfile::tempdir().unwrap();

        let source = NotifyChangeSource::new(&ClaudeHome::new(root.path().to_path_buf())).unwrap();

        std::thread::sleep(Duration::from_millis(50));
        assert!(source.drain().is_empty());
    }
}
