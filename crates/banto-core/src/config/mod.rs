//! banto configuration.
//!
//! `config.toml` lives in `dirs::config_dir()/banto`, data (sqlite) in
//! `dirs::data_local_dir()/banto`; banto never writes outside these two
//! directories of its own. Loading is tolerant: a missing file means all
//! defaults, unknown keys are ignored. A malformed file is an error only for
//! the strict [`load`]; normal startup goes through [`load_or_default`],
//! which falls back to the defaults so a broken config never prevents the
//! TUI from starting (use [`load`] when the error should be surfaced, e.g.
//! for a diagnostics subcommand).

mod paths;

pub use paths::{default_config_path, default_db_path};

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Errors from the strict [`load`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("failed to read config file {path:?}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML (or a field has the wrong shape).
    #[error("failed to parse config file {path:?}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// Which backend resumes sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpenerMode {
    /// Take over banto's own pane: collapse the TUI and run the session as a
    /// child process in the same terminal, no terminal multiplexer involved.
    /// The default — split/tab placement (below) is reserved for the `s`
    /// key.
    #[default]
    InPlace,
    /// Detect a split/tab backend from the environment: `$TMUX` (psmux)
    /// first, then `WT_SESSION`.
    Auto,
    Psmux,
    WindowsTerminal,
}

/// Thresholds for the activity age buckets (plain numbers here; the status
/// module consumes them when wiring happens later).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ActivityConfig {
    /// Sessions modified within this many hours count as "today".
    pub today_hours: u64,
    /// Sessions modified within this many days count as "this week".
    pub week_days: u64,
}

impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            today_hours: 24,
            week_days: 7,
        }
    }
}

/// Brigade formation settings (emporium mode only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BrigadeConfig {
    /// How many fresh Workers to auto-spawn when a brigade is formed.
    /// Clamped to 1..=8 wherever it's consumed — a raw, unclamped value here
    /// lets a config round-trip losslessly even if it's out of range.
    pub workers: u32,
}

impl Default for BrigadeConfig {
    fn default() -> Self {
        Self { workers: 1 }
    }
}

impl BrigadeConfig {
    /// [`Self::workers`] clamped to a sane 1..=8 range for actual use.
    pub fn worker_count(&self) -> usize {
        self.workers.clamp(1, 8) as usize
    }
}

/// Top-level banto configuration. Every field has a default and unknown keys
/// are ignored, so any subset of `config.toml` is valid.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub opener: OpenerMode,
    pub activity: ActivityConfig,
    pub brigade: BrigadeConfig,
    /// Overrides the provider's default `~/.claude` location (read-only!).
    pub claude_home: Option<PathBuf>,
    /// Overrides [`default_db_path`].
    pub db_path: Option<PathBuf>,
}

/// Strict load. A missing file yields all defaults (running without a config
/// file is normal), but an unreadable or unparsable file is an error.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(err) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source: err,
            });
        }
    };
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// Tolerant load for normal startup: any read or parse failure silently falls
/// back to the defaults. See the module docs for the rationale.
pub fn load_or_default(path: &Path) -> Config {
    load(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &tempfile::TempDir, text: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        assert_eq!(load(&path).unwrap(), Config::default());
        assert_eq!(load_or_default(&path), Config::default());
    }

    #[test]
    fn defaults_have_documented_values() {
        let config = Config::default();
        assert_eq!(config.opener, OpenerMode::InPlace);
        assert_eq!(config.activity.today_hours, 24);
        assert_eq!(config.activity.week_days, 7);
        assert_eq!(config.brigade.workers, 1);
        assert_eq!(config.claude_home, None);
        assert_eq!(config.db_path, None);
    }

    #[test]
    fn partial_brigade_section_fills_remaining_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "[brigade]\nworkers = 3\n");
        let config = load(&path).unwrap();
        assert_eq!(config.brigade.workers, 3);
        assert_eq!(config.brigade.worker_count(), 3);
    }

    #[test]
    fn brigade_worker_count_clamps_to_one_through_eight() {
        assert_eq!(BrigadeConfig { workers: 0 }.worker_count(), 1);
        assert_eq!(BrigadeConfig { workers: 1 }.worker_count(), 1);
        assert_eq!(BrigadeConfig { workers: 8 }.worker_count(), 8);
        assert_eq!(BrigadeConfig { workers: 20 }.worker_count(), 8);
    }

    #[test]
    fn partial_toml_fills_remaining_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "opener = \"psmux\"\n");
        let config = load(&path).unwrap();
        assert_eq!(config.opener, OpenerMode::Psmux);
        assert_eq!(config.activity, ActivityConfig::default());
        assert_eq!(config.db_path, None);
    }

    #[test]
    fn partial_activity_section_fills_remaining_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "[activity]\ntoday_hours = 12\n");
        let config = load(&path).unwrap();
        assert_eq!(config.activity.today_hours, 12);
        assert_eq!(config.activity.week_days, 7);
    }

    #[test]
    fn all_opener_values_parse() {
        for (text, expected) in [
            ("in-place", OpenerMode::InPlace),
            ("auto", OpenerMode::Auto),
            ("psmux", OpenerMode::Psmux),
            ("windows-terminal", OpenerMode::WindowsTerminal),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = write_config(&dir, &format!("opener = \"{text}\"\n"));
            assert_eq!(load(&path).unwrap().opener, expected);
        }
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "opener = \"psmux\"\nfuture_option = true\n[some_new_section]\nx = 1\n",
        );
        let config = load(&path).unwrap();
        assert_eq!(config.opener, OpenerMode::Psmux);
    }

    #[test]
    fn path_overrides_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "claude_home = \"C:/synthetic/claude-home\"\ndb_path = \"C:/synthetic/banto.db\"\n",
        );
        let config = load(&path).unwrap();
        assert_eq!(
            config.claude_home,
            Some(PathBuf::from("C:/synthetic/claude-home"))
        );
        assert_eq!(config.db_path, Some(PathBuf::from("C:/synthetic/banto.db")));
    }

    #[test]
    fn invalid_toml_errors_strictly_and_defaults_tolerantly() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "opener = [not toml");
        assert!(matches!(load(&path), Err(ConfigError::Parse { .. })));
        assert_eq!(load_or_default(&path), Config::default());
    }

    #[test]
    fn wrong_field_type_is_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "opener = \"no-such-backend\"\n");
        assert!(matches!(load(&path), Err(ConfigError::Parse { .. })));
        assert_eq!(load_or_default(&path), Config::default());
    }

    #[test]
    fn default_paths_end_in_banto_directory() {
        // Only inspects the computed paths; never creates or writes them.
        if let Some(path) = default_config_path() {
            assert!(path.ends_with(Path::new("banto").join("config.toml")));
        }
        if let Some(path) = default_db_path() {
            assert!(path.ends_with(Path::new("banto").join("banto.db")));
        }
    }
}
