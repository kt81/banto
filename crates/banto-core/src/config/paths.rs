//! Default locations of banto's own files.
//!
//! banto must never write outside its own `banto/` directories under the
//! platform config and local-data dirs (CLAUDE.md invariant 1). Everything
//! under `~/.claude` is read-only for banto.

use std::path::PathBuf;

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
