//! Loading `banto_core::config`'s types from disk.
//!
//! `config.toml`'s location is resolved by [`resolve_config_path`], first
//! hit wins: an explicit `--config`/`BANTO_CONFIG` override, then
//! `$XDG_CONFIG_HOME/banto/config.toml`, then `~/.config/banto/config.toml`,
//! then the platform default `dirs::config_dir()/banto/config.toml`. Data
//! (sqlite) lives in `dirs::data_local_dir()/banto`; banto never writes
//! outside these two directories of its own.
//!
//! Loading is tolerant by default: a missing file means all defaults,
//! unknown keys are ignored. A malformed file is an error only for the
//! strict [`load`]; normal startup goes through [`load_or_default`], which
//! falls back to the defaults so a broken config never prevents the TUI
//! from starting (use [`load`] when the error should be surfaced, e.g. for
//! a diagnostics subcommand). The two explicit overrides are stricter still
//! — see [`load_explicit`]: a missing file is *also* an error there, since
//! silently falling back would hide a typo in a path the operator named on
//! purpose.

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

/// Errors from the strict [`load`] and [`load_explicit`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file does not exist. Only [`load_explicit`] treats this as an
    /// error; [`load`] treats a missing file as "use the defaults".
    #[error("config file not found: {path:?}")]
    Missing { path: PathBuf },
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

/// Reads and parses `path`, distinguishing "file does not exist" (`Ok(None)`)
/// from every other outcome, so [`load`] and [`load_explicit`] can each
/// decide what a missing file means without duplicating the read/parse
/// logic.
fn read_and_parse(path: &Path) -> Result<Option<Config>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source: err,
            });
        }
    };
    let config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(config))
}

/// Strict load. A missing file yields all defaults (running without a config
/// file is normal), but an unreadable or unparsable file is an error.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    Ok(read_and_parse(path)?.unwrap_or_default())
}

/// Load for an explicit override (`--config` / `BANTO_CONFIG`): unlike
/// [`load`], a missing file is *also* an error — the operator named this
/// path on purpose, so silently falling back to defaults would hide a typo.
pub fn load_explicit(path: &Path) -> Result<Config, ConfigError> {
    read_and_parse(path)?.ok_or_else(|| ConfigError::Missing {
        path: path.to_path_buf(),
    })
}

/// Tolerant load for normal startup: any read or parse failure silently falls
/// back to the defaults. See the module docs for the rationale.
pub fn load_or_default(path: &Path) -> Config {
    load(path).unwrap_or_default()
}

/// Which tier of the resolution order in [`resolve_config_path`] produced a
/// config path, and therefore how strictly it should be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// `--config` or `BANTO_CONFIG`: named explicitly by the operator, so it
    /// must exist and parse — load with [`load_explicit`].
    Explicit(PathBuf),
    /// `$XDG_CONFIG_HOME/banto/config.toml` or `~/.config/banto/config.toml`,
    /// found by existence probing. Loaded leniently, like the platform
    /// default — only the two explicit overrides above are strict.
    Discovered(PathBuf),
    /// `dirs::config_dir()/banto/config.toml`, the final, unconditional
    /// fallback. `None` only when the platform has no config dir at all.
    Default(Option<PathBuf>),
}

/// Resolves which `config.toml` banto should load, first hit wins:
///
/// 1. `cli_override` (`--config`) — must exist, loaded via [`load_explicit`].
/// 2. `env_override` (`BANTO_CONFIG`) — same strictness.
/// 3. `$XDG_CONFIG_HOME/banto/config.toml`, honored on every platform
///    (including Windows, for operators who set it deliberately) when
///    `xdg_config_home` is set and non-empty *and* the file exists.
/// 4. `~/.config/banto/config.toml` (`home`-based), on every platform, if it
///    exists.
/// 5. [`default_config_path`], unconditionally — the current behavior,
///    unchanged: a missing or broken file there just means defaults.
///
/// Takes its environment/home inputs as parameters rather than reading them
/// itself, so this stays a pure function of its arguments and tests need no
/// real environment mutation; `main.rs` passes the real
/// `std::env::var_os`/`dirs::home_dir()` values in.
pub fn resolve_config_path(
    cli_override: Option<&Path>,
    env_override: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> ConfigSource {
    if let Some(path) = cli_override {
        return ConfigSource::Explicit(path.to_path_buf());
    }
    if let Some(path) = env_override {
        return ConfigSource::Explicit(path.to_path_buf());
    }
    if let Some(xdg) = xdg_config_home.filter(|path| !path.as_os_str().is_empty()) {
        let candidate = xdg.join("banto").join("config.toml");
        if candidate.exists() {
            return ConfigSource::Discovered(candidate);
        }
    }
    if let Some(home) = home {
        let candidate = home.join(".config").join("banto").join("config.toml");
        if candidate.exists() {
            return ConfigSource::Discovered(candidate);
        }
    }
    ConfigSource::Default(default_config_path())
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

    #[test]
    fn load_explicit_errors_on_a_missing_file_unlike_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        assert_eq!(load(&path).unwrap(), Config::default());
        assert!(matches!(
            load_explicit(&path),
            Err(ConfigError::Missing { .. })
        ));
    }

    #[test]
    fn load_explicit_round_trips_an_existing_file_like_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "opener = \"psmux\"\n");
        assert_eq!(load_explicit(&path).unwrap(), load(&path).unwrap());
    }

    #[test]
    fn load_explicit_still_surfaces_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "opener = [not toml");
        assert!(matches!(
            load_explicit(&path),
            Err(ConfigError::Parse { .. })
        ));
    }

    // -- resolve_config_path -------------------------------------------

    fn banto_dir(root: &Path) -> PathBuf {
        root.join("banto")
    }

    fn touch_config(root: &Path) -> PathBuf {
        let dir = banto_dir(root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "").unwrap();
        path
    }

    #[test]
    fn explicit_cli_override_wins_over_everything_even_if_nothing_else_exists() {
        let cli = PathBuf::from("/synthetic/cli-config.toml");
        let source = resolve_config_path(Some(&cli), None, None, None);
        assert_eq!(source, ConfigSource::Explicit(cli));
    }

    #[test]
    fn explicit_env_override_wins_when_no_cli_override_is_given() {
        let env = PathBuf::from("/synthetic/env-config.toml");
        let source = resolve_config_path(None, Some(&env), None, None);
        assert_eq!(source, ConfigSource::Explicit(env));
    }

    #[test]
    fn cli_override_beats_env_override_when_both_are_given() {
        let cli = PathBuf::from("/synthetic/cli-config.toml");
        let env = PathBuf::from("/synthetic/env-config.toml");
        let source = resolve_config_path(Some(&cli), Some(&env), None, None);
        assert_eq!(source, ConfigSource::Explicit(cli));
    }

    #[test]
    fn xdg_config_home_is_used_when_set_non_empty_and_the_file_exists() {
        let xdg_root = tempfile::tempdir().unwrap();
        let expected = touch_config(xdg_root.path());

        let source = resolve_config_path(None, None, Some(xdg_root.path()), None);
        assert_eq!(source, ConfigSource::Discovered(expected));
    }

    #[test]
    fn xdg_config_home_is_ignored_when_the_file_does_not_exist_there() {
        let xdg_root = tempfile::tempdir().unwrap();
        // banto/config.toml is never created under xdg_root.

        let source = resolve_config_path(None, None, Some(xdg_root.path()), None);
        assert_eq!(source, ConfigSource::Default(default_config_path()));
    }

    #[test]
    fn xdg_config_home_set_to_an_empty_path_is_treated_as_unset() {
        let xdg_root = tempfile::tempdir().unwrap();
        touch_config(xdg_root.path());

        let source = resolve_config_path(None, None, Some(Path::new("")), None);
        assert_eq!(source, ConfigSource::Default(default_config_path()));
    }

    #[test]
    fn xdg_config_home_beats_home_dot_config_when_both_have_the_file() {
        let xdg_root = tempfile::tempdir().unwrap();
        let home_root = tempfile::tempdir().unwrap();
        let expected = touch_config(xdg_root.path());
        std::fs::create_dir_all(home_root.path().join(".config").join("banto")).unwrap();
        std::fs::write(
            home_root
                .path()
                .join(".config")
                .join("banto")
                .join("config.toml"),
            "",
        )
        .unwrap();

        let source = resolve_config_path(None, None, Some(xdg_root.path()), Some(home_root.path()));
        assert_eq!(source, ConfigSource::Discovered(expected));
    }

    #[test]
    fn home_dot_config_is_used_when_xdg_is_unset_and_the_file_exists() {
        let home_root = tempfile::tempdir().unwrap();
        let dotconfig = home_root.path().join(".config");
        let dir = dotconfig.join("banto");
        std::fs::create_dir_all(&dir).unwrap();
        let expected = dir.join("config.toml");
        std::fs::write(&expected, "").unwrap();

        let source = resolve_config_path(None, None, None, Some(home_root.path()));
        assert_eq!(source, ConfigSource::Discovered(expected));
    }

    #[test]
    fn home_dot_config_is_ignored_when_the_file_does_not_exist_there() {
        let home_root = tempfile::tempdir().unwrap();
        // ~/.config/banto/config.toml is never created under home_root.

        let source = resolve_config_path(None, None, None, Some(home_root.path()));
        assert_eq!(source, ConfigSource::Default(default_config_path()));
    }

    #[test]
    fn falls_back_to_the_platform_default_when_nothing_else_is_set() {
        let source = resolve_config_path(None, None, None, None);
        assert_eq!(source, ConfigSource::Default(default_config_path()));
    }
}
