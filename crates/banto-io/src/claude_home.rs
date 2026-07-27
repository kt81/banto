//! The root directory Claude Code stores its own state under (`~/.claude`
//! by default, or `$CLAUDE_CONFIG_DIR` when set — Claude Code's own
//! override variable, e.g. for a multi-account setup:
//! <https://github.com/anthropics/claude-code/issues/28808>), and the two
//! subdirectories banto reads from it.
//!
//! Pure paths only: no I/O, no existence checks, no failure mode of its own.
//! [`crate::status`] deliberately does not depend on this type — it stays
//! product-agnostic, taking a plain `&Path` for wherever it's told to read
//! live-state files from; callers resolve [`ClaudeHome::sessions_dir`] and
//! pass the resulting path in.

use std::path::{Path, PathBuf};

/// The directory Claude Code stores its own state under. Named for Claude
/// Code specifically, not a generic "agent home": a different product's
/// session store can have an entirely different layout (no `projects/`, no
/// `sessions/<pid>.json`) and will get its own type when that lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeHome(PathBuf);

impl ClaudeHome {
    /// Wrap an explicit root (from `--claude-home` or `config.toml`'s
    /// `claude_home`).
    pub fn new(root: PathBuf) -> Self {
        Self(root)
    }

    /// The default root: `env("CLAUDE_CONFIG_DIR")` if set and non-empty,
    /// else `~/.claude`, if a home directory exists. `env` is injected — the
    /// house pattern for environment-dependent code (see
    /// `CodexHome::default_home_from_env`) — so tests never mutate the real
    /// process environment.
    pub fn default_home_from_env(env: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if let Some(from_env) = env("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
            return Some(Self(PathBuf::from(from_env)));
        }
        dirs::home_dir().map(|home| Self(home.join(".claude")))
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

    /// `<root>/projects`: session `.jsonl` files (provider discovery).
    pub fn projects_dir(&self) -> PathBuf {
        self.0.join("projects")
    }

    /// `<root>/sessions`: live-state `<pid>.json` files (activity).
    pub fn sessions_dir(&self) -> PathBuf {
        self.0.join("sessions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_projects_and_sessions_dirs_join_onto_the_wrapped_root() {
        let home = ClaudeHome::new(PathBuf::from("/synthetic/claude-home"));
        assert_eq!(home.root(), Path::new("/synthetic/claude-home"));
        assert_eq!(
            home.projects_dir(),
            PathBuf::from("/synthetic/claude-home/projects")
        );
        assert_eq!(
            home.sessions_dir(),
            PathBuf::from("/synthetic/claude-home/sessions")
        );
    }

    #[test]
    fn default_home_ends_with_dot_claude() {
        // Only inspects the constructed path; never reads the real ~/.claude.
        if let Some(home) = ClaudeHome::default_home() {
            assert!(home.root().ends_with(".claude"));
        }
    }

    #[test]
    fn default_home_from_env_honors_claude_config_dir_when_set() {
        let env =
            |key: &str| (key == "CLAUDE_CONFIG_DIR").then(|| "/synthetic/claude-home".to_string());
        assert_eq!(
            ClaudeHome::default_home_from_env(env),
            Some(ClaudeHome::new(PathBuf::from("/synthetic/claude-home")))
        );
    }

    #[test]
    fn default_home_from_env_ignores_an_empty_claude_config_dir() {
        let env = |key: &str| (key == "CLAUDE_CONFIG_DIR").then(String::new);
        // Falls through to the real ~/.claude; only inspects the constructed
        // path, never reads it.
        if let Some(home) = ClaudeHome::default_home_from_env(env) {
            assert!(home.root().ends_with(".claude"));
        }
    }

    #[test]
    fn default_home_from_env_falls_back_to_dot_claude_when_unset() {
        let env = |_: &str| None;
        if let Some(home) = ClaudeHome::default_home_from_env(env) {
            assert!(home.root().ends_with(".claude"));
        }
    }
}
