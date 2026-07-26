//! The root directory the Codex CLI stores its own state under (`~/.codex`
//! by default, or `$CODEX_HOME` when set — Codex's own override variable;
//! Claude Code has none, so [`ClaudeHome`](crate::claude_home::ClaudeHome)
//! only ever takes an explicit root).
//!
//! Codex's layout is nothing like Claude's: no `projects/`, no
//! `sessions/<pid>.json`. A sqlite database (`state_5.sqlite`, observed on
//! this machine's installed Codex CLI version — the filename may change
//! across versions) holds a `threads` table indexing every session, each row
//! naming a `rollout_path` — the actual per-session transcript file. Its
//! `sessions/` tree has no accessor here because nothing joins paths onto
//! it: discovery reads each row's `rollout_path` from the table directly.
//!
//! Pure paths only: no I/O, no existence checks, no failure mode of its own
//! — same contract as [`ClaudeHome`](crate::claude_home::ClaudeHome).

use std::path::{Path, PathBuf};

/// The directory the Codex CLI stores its own state under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHome(PathBuf);

impl CodexHome {
    /// Wrap an explicit root.
    pub fn new(root: PathBuf) -> Self {
        Self(root)
    }

    /// The default root: `env("CODEX_HOME")` if set and non-empty, else
    /// `~/.codex`, if a home directory exists. `env` is injected — the house
    /// pattern for testing environment-dependent code (see
    /// `opener::detect_backend`'s own `env` parameter) — so tests never
    /// mutate the real process environment.
    pub fn default_home_from_env(env: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if let Some(from_env) = env("CODEX_HOME").filter(|v| !v.is_empty()) {
            return Some(Self(PathBuf::from(from_env)));
        }
        dirs::home_dir().map(|home| Self(home.join(".codex")))
    }

    /// [`Self::default_home_from_env`] against the real process environment,
    /// for a caller with no injected value of its own.
    pub fn default_home() -> Option<Self> {
        Self::default_home_from_env(|key| std::env::var(key).ok())
    }

    /// The root itself.
    pub fn root(&self) -> &Path {
        &self.0
    }

    /// `<root>/state_5.sqlite`: the `threads` table, one row per session —
    /// see [`provider::codex`] for the columns banto reads.
    pub fn threads_db_path(&self) -> PathBuf {
        self.0.join("state_5.sqlite")
    }

    /// `<root>/logs_2.sqlite`: process liveness (`logs.process_uuid`,
    /// `pid:<PID>:<suffix>`) — not read by discovery; here for the liveness
    /// round that follows, which must independently re-verify its own
    /// version of the sqlite-safety question rather than assuming
    /// `threads_db_path`'s answer transfers to a different database file.
    pub fn logs_db_path(&self) -> PathBuf {
        self.0.join("logs_2.sqlite")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_join_onto_the_wrapped_root() {
        let home = CodexHome::new(PathBuf::from("/synthetic/codex-home"));
        assert_eq!(home.root(), Path::new("/synthetic/codex-home"));
        assert_eq!(
            home.threads_db_path(),
            PathBuf::from("/synthetic/codex-home/state_5.sqlite")
        );
        assert_eq!(
            home.logs_db_path(),
            PathBuf::from("/synthetic/codex-home/logs_2.sqlite")
        );
    }

    #[test]
    fn default_home_from_env_honors_codex_home_when_set() {
        let env = |key: &str| (key == "CODEX_HOME").then(|| "/synthetic/codex-home".to_string());
        assert_eq!(
            CodexHome::default_home_from_env(env),
            Some(CodexHome::new(PathBuf::from("/synthetic/codex-home")))
        );
    }

    #[test]
    fn default_home_from_env_ignores_an_empty_codex_home() {
        let env = |key: &str| (key == "CODEX_HOME").then(String::new);
        // Falls through to the real ~/.codex; only inspects the constructed
        // path, never reads it.
        if let Some(home) = CodexHome::default_home_from_env(env) {
            assert!(home.root().ends_with(".codex"));
        }
    }

    #[test]
    fn default_home_from_env_falls_back_to_dot_codex_when_unset() {
        let env = |_: &str| None;
        if let Some(home) = CodexHome::default_home_from_env(env) {
            assert!(home.root().ends_with(".codex"));
        }
    }
}
