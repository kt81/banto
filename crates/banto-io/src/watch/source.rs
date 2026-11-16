//! The real filesystem watcher, isolated behind [`ChangeSource`] so callers
//! (and their tests) depend on the trait rather than on `notify` directly.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::SystemTime;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

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
/// (recursively) and its index in the home directory, tagging observed changes
/// with the [`WatchRoot`] they fall under.
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
    /// The Codex home is watched non-recursively to catch index creation and
    /// replacement. Only events naming `session_index.jsonl` trigger reloads;
    /// SQLite and log activity is excluded.
    pub fn new(
        claude_home: &ClaudeHome,
        codex_home: Option<&CodexHome>,
    ) -> Result<Self, WatchError> {
        let roots = WatchRoots::new(claude_home, codex_home);
        let (tx, rx) = mpsc::channel();
        let watch_roots = roots.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };
            let now = SystemTime::now();
            let index_changed = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            );
            for root in classify_paths(&event.paths, &watch_roots, index_changed) {
                // A send error means the receiving end was dropped (the
                // source is going away); nothing to do about it here.
                let _ = tx.send(RawChange { root, at: now });
            }
        })?;

        register_roots(&roots, |path, mode| watcher.watch(path, mode))?;

        Ok(Self {
            _watcher: watcher,
            changes: rx,
        })
    }
}

fn register_roots(
    roots: &WatchRoots,
    mut watch: impl FnMut(&Path, RecursiveMode) -> notify::Result<()>,
) -> notify::Result<()> {
    if roots.projects.given.is_dir() {
        watch(&roots.projects.given, RecursiveMode::Recursive)?;
    }
    if roots.sessions.given.is_dir() {
        watch(&roots.sessions.given, RecursiveMode::NonRecursive)?;
    }
    if let Some(root) = &roots.codex_sessions
        && root.given.is_dir()
    {
        watch(&root.given, RecursiveMode::Recursive)?;
    }
    if let Some(root) = &roots.codex_home
        && root.given.is_dir()
    {
        let _ = watch(&root.given, RecursiveMode::NonRecursive);
    }
    Ok(())
}

impl ChangeSource for NotifyChangeSource {
    fn drain(&self) -> Vec<RawChange> {
        self.changes.try_iter().collect()
    }
}

#[derive(Clone)]
struct WatchedRoot {
    given: PathBuf,
    canonical: Option<PathBuf>,
}

impl WatchedRoot {
    fn new(given: PathBuf) -> Self {
        let canonical = given.canonicalize().ok();
        Self { given, canonical }
    }

    fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.given)
            || self
                .canonical
                .as_ref()
                .is_some_and(|root| path.starts_with(root))
    }
}

#[derive(Clone)]
struct WatchRoots {
    projects: WatchedRoot,
    sessions: WatchedRoot,
    codex_sessions: Option<WatchedRoot>,
    codex_home: Option<WatchedRoot>,
}

impl WatchRoots {
    fn new(claude_home: &ClaudeHome, codex_home: Option<&CodexHome>) -> Self {
        Self {
            projects: WatchedRoot::new(claude_home.projects_dir()),
            sessions: WatchedRoot::new(claude_home.sessions_dir()),
            codex_sessions: codex_home.map(|home| WatchedRoot::new(home.rollout_dir())),
            codex_home: codex_home.map(|home| WatchedRoot::new(home.root().to_path_buf())),
        }
    }
}

// FSEvents reports canonical paths; Windows can report the original spelling
// even though canonicalize adds a verbatim prefix there, so both must match.
fn classify_paths(paths: &[PathBuf], watched: &WatchRoots, index_changed: bool) -> Vec<WatchRoot> {
    let mut roots = Vec::new();
    for path in paths {
        let root = if watched.projects.contains(path) {
            WatchRoot::Projects
        } else if watched.sessions.contains(path) {
            WatchRoot::Sessions
        } else if watched
            .codex_sessions
            .as_ref()
            .is_some_and(|root| root.contains(path))
            || (index_changed
                && watched.codex_home.as_ref().is_some_and(|root| {
                    *path == root.given.join("session_index.jsonl")
                        || root
                            .canonical
                            .as_ref()
                            .is_some_and(|canonical| *path == canonical.join("session_index.jsonl"))
                }))
        {
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

    fn synthetic_roots() -> WatchRoots {
        WatchRoots {
            projects: WatchedRoot {
                given: "given/projects".into(),
                canonical: Some("canonical/projects".into()),
            },
            sessions: WatchedRoot {
                given: "given/sessions".into(),
                canonical: Some("canonical/sessions".into()),
            },
            codex_home: None,
            codex_sessions: Some(WatchedRoot {
                given: "given/codex".into(),
                canonical: Some("canonical/codex".into()),
            }),
        }
    }

    #[test]
    fn classifies_given_and_canonical_root_spellings() {
        let roots = synthetic_roots();
        for (path, expected) in [
            ("given/projects/a", WatchRoot::Projects),
            ("canonical/projects/a", WatchRoot::Projects),
            ("given/sessions/a", WatchRoot::Sessions),
            ("canonical/sessions/a", WatchRoot::Sessions),
            ("given/codex/a", WatchRoot::CodexSessions),
            ("canonical/codex/a", WatchRoot::CodexSessions),
        ] {
            assert_eq!(classify_paths(&[path.into()], &roots, true), vec![expected]);
        }
        assert!(classify_paths(&["unrelated/a".into()], &roots, true).is_empty());
        assert!(classify_paths(&["given/projects-other/a".into()], &roots, true).is_empty());
    }

    #[test]
    fn missing_root_keeps_its_given_spelling() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let root = WatchedRoot::new(missing.clone());
        assert!(root.canonical.is_none());
        assert!(root.contains(&missing.join("file")));
    }

    #[test]
    fn overlapping_roots_keep_existing_priority_and_deduplicate() {
        let mut roots = synthetic_roots();
        roots.sessions.given = "given/projects/nested".into();
        roots.codex_sessions.as_mut().unwrap().canonical = Some("canonical/sessions/nested".into());
        assert_eq!(
            classify_paths(
                &["given/projects/nested/a".into(), "given/projects/b".into()],
                &roots,
                true
            ),
            vec![WatchRoot::Projects]
        );
        assert_eq!(
            classify_paths(&["canonical/sessions/nested/a".into()], &roots, true),
            vec![WatchRoot::Sessions]
        );
    }

    #[cfg(unix)]
    #[test]
    fn registered_symlink_root_accepts_event_under_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir_all(target.join("projects")).unwrap();
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();
        let home = ClaudeHome::new(link.clone());
        let _source = NotifyChangeSource::new(&home, None).unwrap();
        let roots = WatchRoots::new(&home, None);
        let canonical_event = target
            .join("projects/a")
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("a");
        assert_eq!(
            classify_paths(&[canonical_event], &roots, true),
            vec![WatchRoot::Projects]
        );
    }

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
    #[test]
    fn detects_index_created_after_watcher_start_and_replaced() {
        let claude = tempfile::tempdir().unwrap();
        let codex = tempfile::tempdir().unwrap();
        let source = NotifyChangeSource::new(
            &ClaudeHome::new(claude.path().into()),
            Some(&CodexHome::new(codex.path().into())),
        )
        .unwrap();
        let index = codex.path().join("session_index.jsonl");
        fs::write(&index, "{}\n").unwrap();
        assert!(wait_for(
            &source,
            WatchRoot::CodexSessions,
            Duration::from_secs(2)
        ));
        std::thread::sleep(Duration::from_millis(100));
        source.drain();
        let replacement = codex.path().join("replacement.tmp");
        fs::write(&replacement, "{}\n").unwrap();
        fs::remove_file(&index).unwrap();
        assert!(wait_for(
            &source,
            WatchRoot::CodexSessions,
            Duration::from_secs(2)
        ));
        std::thread::sleep(Duration::from_millis(100));
        source.drain();
        fs::rename(&replacement, &index).unwrap();
        assert!(wait_for(
            &source,
            WatchRoot::CodexSessions,
            Duration::from_secs(2)
        ));
    }
    #[test]
    fn index_filter_ignores_access_and_unrelated_files() {
        let mut roots = synthetic_roots();
        roots.codex_home = Some(WatchedRoot {
            given: "given/home".into(),
            canonical: Some("canonical/home".into()),
        });
        for p in [
            "given/home/session_index.jsonl",
            "canonical/home/session_index.jsonl",
        ] {
            assert_eq!(
                classify_paths(&[p.into()], &roots, true),
                vec![WatchRoot::CodexSessions]
            );
            assert!(classify_paths(&[p.into()], &roots, false).is_empty());
        }
        for p in [
            "given/home/state_5.sqlite",
            "given/home/logs_2.sqlite-wal",
            "given/home/session_index.jsonl.tmp",
            "given/home/sub/session_index.jsonl",
        ] {
            assert!(classify_paths(&[p.into()], &roots, true).is_empty());
        }
        roots.codex_home = None;
        assert!(
            classify_paths(&["given/home/session_index.jsonl".into()], &roots, true).is_empty()
        );
    }
    #[test]
    fn index_watch_failure_does_not_disable_existing_watches() {
        let claude = tempfile::tempdir().unwrap();
        let codex = tempfile::tempdir().unwrap();
        fs::create_dir_all(claude.path().join("projects")).unwrap();
        fs::create_dir_all(claude.path().join("sessions")).unwrap();
        fs::create_dir_all(codex.path().join("sessions")).unwrap();
        let roots = WatchRoots::new(
            &ClaudeHome::new(claude.path().into()),
            Some(&CodexHome::new(codex.path().into())),
        );
        let mut registered = Vec::new();
        let result = register_roots(&roots, |path, _| {
            if path == codex.path() {
                return Err(notify::Error::generic("denied"));
            }
            registered.push(path.to_path_buf());
            Ok(())
        });
        assert!(result.is_ok());
        assert_eq!(registered.len(), 3);
    }
}
