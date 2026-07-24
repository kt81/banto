//! Loading `banto_core::config`'s types from disk.
//!
//! `config.toml` lives in `dirs::config_dir()/banto`, data (sqlite) in
//! `dirs::data_local_dir()/banto`; banto never writes outside these two
//! directories of its own. Loading is tolerant: a missing file means all
//! defaults, unknown keys are ignored. A malformed file is an error only for
//! the strict [`load`]; normal startup goes through [`load_or_default`],
//! which falls back to the defaults so a broken config never prevents the
//! TUI from starting (use [`load`] when the error should be surfaced, e.g.
//! for a diagnostics subcommand).

use std::path::{Path, PathBuf};

pub use banto_core::config::{
    ActivityConfig, BrigadeConfig, Config, KeysConfig, OpenerMode, RelayMode,
};

/// `dirs::config_dir()/banto/config.toml`
/// (e.g. `%APPDATA%\banto\config.toml` on Windows,
/// `~/.config/banto/config.toml` on Linux).
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("banto").join("config.toml"))
}

/// `dirs::data_local_dir()/banto/banto.db`
/// (e.g. `%LOCALAPPDATA%\banto\banto.db` on Windows,
/// `~/.local/share/banto/banto.db` on Linux).
pub fn default_db_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("banto").join("banto.db"))
}

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
    fn a_populated_file_round_trips_through_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "opener = \"psmux\"\n[brigade]\nworkers = 3\nworker_model = \"opus\"\n",
        );
        let config = load(&path).unwrap();
        assert_eq!(config.opener, OpenerMode::Psmux);
        assert_eq!(config.brigade.workers, 3);
        assert_eq!(config.brigade.worker_model, "opus");
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
