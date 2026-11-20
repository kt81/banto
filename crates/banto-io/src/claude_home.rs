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
pub struct ClaudeHome {
    root: PathBuf,
    /// Where the agent in a pane keeps its `.claude.json`, when the caller
    /// knows; see [`Self::trust_registry_path`].
    trust_registry: Option<PathBuf>,
}

/// Claude Code's own name for its global state file, `~/.claude.json`.
const TRUST_REGISTRY_FILE: &str = ".claude.json";

impl ClaudeHome {
    /// Wrap an explicit root (from `--claude-home` or `config.toml`'s
    /// `claude_home`).
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            trust_registry: None,
        }
    }

    /// Say where the agent keeps its `.claude.json` (see
    /// [`Self::agent_trust_registry_from_env`]), for a root that is not
    /// simply `<home>/.claude`.
    #[must_use]
    pub fn with_trust_registry(mut self, path: PathBuf) -> Self {
        self.trust_registry = Some(path);
        self
    }

    /// The default root: `env("CLAUDE_CONFIG_DIR")` if set and non-empty,
    /// else `~/.claude`, if a home directory exists. `env` is injected — the
    /// house pattern for environment-dependent code (see
    /// `CodexHome::default_home_from_env`) — so tests never mutate the real
    /// process environment.
    pub fn default_home_from_env(env: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if let Some(from_env) = env("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
            let root = PathBuf::from(from_env);
            let registry = root.join(TRUST_REGISTRY_FILE);
            return Some(Self::new(root).with_trust_registry(registry));
        }
        dirs::home_dir().map(|home| Self::new(home.join(".claude")))
    }

    /// [`Self::default_home_from_env`] against the real process environment,
    /// for a caller with no injected value of its own.
    pub fn default_home() -> Option<Self> {
        Self::default_home_from_env(|key| std::env::var(key).ok())
    }

    /// The root itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/projects`: session `.jsonl` files (provider discovery).
    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    /// `<root>/sessions`: live-state `<pid>.json` files (activity).
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// `<root>/.last-update-result.json`: Claude Code's record of its most
    /// recent automatic update attempt.  This is not an installed-version
    /// probe; see [`crate::agent_versions`] for that deliberately narrower
    /// fact.
    pub fn last_update_result_path(&self) -> PathBuf {
        self.root.join(".last-update-result.json")
    }

    /// The `.claude.json` the agent consults for per-directory trust (see
    /// `crate::directory_trust`): the one the caller named with
    /// [`Self::with_trust_registry`], else the sibling of `<root>`, which is
    /// right only for the default layout where `<root>` is `<home>/.claude`
    /// and the file is `<home>/.claude.json`.
    ///
    /// The file is **not** a function of `<root>`: it is wherever the agent
    /// *in the pane* looks, and that agent inherits banto's environment, not
    /// banto's `--claude-home`. A root banto was told about can therefore
    /// differ from the home its panes use; the startup resolution names the
    /// file by [`Self::agent_trust_registry_from_env`] instead of deriving it.
    pub fn trust_registry_path(&self) -> PathBuf {
        if let Some(path) = &self.trust_registry {
            return path.clone();
        }
        match self.root.parent() {
            Some(parent) => parent.join(TRUST_REGISTRY_FILE),
            None => self.root.join(TRUST_REGISTRY_FILE),
        }
    }

    /// Where an agent started from this environment keeps `.claude.json`:
    /// `$CLAUDE_CONFIG_DIR/.claude.json` when the variable is set and
    /// non-empty, else `<home>/.claude.json`. Claude Code's docs say the file
    /// follows the variable ("If you set CLAUDE_CONFIG_DIR, every ~/.claude
    /// path on this page lives under that directory instead. The same applies
    /// to .claude.json", <https://code.claude.com/docs/en/claude-directory>),
    /// so `CLAUDE_CONFIG_DIR=~/.claude-work` puts it at
    /// `~/.claude-work/.claude.json`, not in `~/`, where another account's
    /// file may sit. `env` is injected as in [`Self::default_home_from_env`].
    pub fn agent_trust_registry_from_env(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
        match env("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => Some(PathBuf::from(dir).join(TRUST_REGISTRY_FILE)),
            None => dirs::home_dir().map(|home| home.join(TRUST_REGISTRY_FILE)),
        }
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
        assert_eq!(
            home.last_update_result_path(),
            PathBuf::from("/synthetic/claude-home/.last-update-result.json")
        );
        assert_eq!(
            home.trust_registry_path(),
            PathBuf::from("/synthetic/.claude.json")
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
        let home = ClaudeHome::default_home_from_env(env).unwrap();
        assert_eq!(home.root(), Path::new("/synthetic/claude-home"));
        // The variable moves `.claude.json` into the directory with it.
        assert_eq!(
            home.trust_registry_path(),
            PathBuf::from("/synthetic/claude-home/.claude.json")
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

    #[test]
    fn the_default_home_keeps_its_registry_beside_it() {
        let home = ClaudeHome::new(PathBuf::from("/synthetic/user/.claude"));
        assert_eq!(
            home.trust_registry_path(),
            PathBuf::from("/synthetic/user/.claude.json")
        );
    }

    #[test]
    fn a_named_registry_wins_over_the_sibling_of_the_root() {
        let home = ClaudeHome::new(PathBuf::from("/synthetic/user/.claude-work"))
            .with_trust_registry(PathBuf::from("/synthetic/elsewhere/.claude.json"));
        assert_eq!(
            home.trust_registry_path(),
            PathBuf::from("/synthetic/elsewhere/.claude.json")
        );
        // Only the registry moves.
        assert_eq!(
            home.projects_dir(),
            PathBuf::from("/synthetic/user/.claude-work/projects")
        );
    }

    #[test]
    fn the_agents_registry_follows_claude_config_dir_into_that_directory() {
        let env = |key: &str| (key == "CLAUDE_CONFIG_DIR").then(|| "/synthetic/work".to_string());
        assert_eq!(
            ClaudeHome::agent_trust_registry_from_env(env),
            Some(PathBuf::from("/synthetic/work/.claude.json"))
        );
    }

    #[test]
    fn the_agents_registry_is_in_the_user_home_when_the_variable_is_unset_or_empty() {
        // Inspects the constructed path only; never reads the real file.
        for value in [None, Some(String::new())] {
            let env = |_: &str| value.clone();
            if let Some(path) = ClaudeHome::agent_trust_registry_from_env(env) {
                assert!(path.ends_with(".claude.json"));
                assert_ne!(path.parent(), Some(Path::new("")));
                assert_eq!(path.parent(), dirs::home_dir().as_deref());
            }
        }
    }
}
