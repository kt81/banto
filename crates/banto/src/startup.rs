//! Where a banto process finds its own files: the `config.toml` it loads, its
//! database, and the Claude and Codex homes it reads — decided in one place, as
//! a function of plain inputs (the command line, the environment, the working
//! directory), so the decision can be tested without touching a real directory
//! and so a process banto starts can be made to reach the same answer as the
//! banto that started it.
//!
//! Relative paths are made absolute against the working directory at the time
//! of resolution. That is what a relative `--config`, `BANTO_CONFIG`,
//! `db_path` or home has always meant to the process that read it; it is only
//! lost on the way to a child, whose own working directory is the session's.
//! An empty path is left as it is (it has never named the working directory), and
//! so is every relative path when the working directory cannot be read.
//!
//! A banto that starts other banto processes (`_mcp`, `_hook`) hands
//! them what it resolved — [`Resolved::child_args`] on their command lines,
//! [`Resolved::child_env`] for the one that cannot take arguments — and a child
//! that is given them resolves exactly those, consulting neither its own
//! environment nor its own working directory. A child started without them
//! (a hand-run `banto _mcp`, an MCP config written by an older banto) resolves
//! for itself, as before.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use banto_core::config::Config;
use banto_io::claude_home::ClaudeHome;
use banto_io::codex_home::CodexHome;
use banto_io::config as io_config;
use banto_io::data_dir::DataDir;

/// The environment a resolution reads, as plain values — never the process
/// environment itself, so a test names exactly what it depends on.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// `BANTO_CONFIG`.
    pub banto_config: Option<PathBuf>,
    /// `XDG_CONFIG_HOME`.
    pub xdg_config_home: Option<PathBuf>,
    /// The user's home directory (`dirs::home_dir()`).
    pub home: Option<PathBuf>,
    /// `CLAUDE_CONFIG_DIR`.
    pub claude_config_dir: Option<String>,
    /// `CODEX_HOME`.
    pub codex_home: Option<String>,
    /// The platform's `config.toml` location, the last tier of the search.
    pub default_config: Option<PathBuf>,
    /// The platform's data directory for banto (`<data_local>/banto`): where the
    /// database lives by default, and where banto keeps its MCP configs,
    /// consultation requests and message logs whatever the database path is.
    pub data_dir: Option<PathBuf>,
}

impl Env {
    /// The real process environment and platform directories.
    pub fn from_process() -> Self {
        Self {
            banto_config: std::env::var_os("BANTO_CONFIG").map(PathBuf::from),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            home: dirs::home_dir(),
            claude_config_dir: std::env::var("CLAUDE_CONFIG_DIR").ok(),
            codex_home: std::env::var("CODEX_HOME").ok(),
            default_config: io_config::default_config_path(),
            data_dir: DataDir::default_root(),
        }
    }

    /// A product's default home: its own override variable when set and
    /// non-empty, else `<home>/<dot_dir>` — the rule `ClaudeHome::default_home`
    /// and `CodexHome::default_home` apply, but over this `Env` so a test never
    /// reaches the real home directory.
    fn default_home(&self, variable: &str, dot_dir: &str, cwd: Option<&Path>) -> Option<PathBuf> {
        let from_env = match variable {
            "CLAUDE_CONFIG_DIR" => &self.claude_config_dir,
            _ => &self.codex_home,
        };
        match from_env.as_deref().filter(|value| !value.is_empty()) {
            Some(value) => Some(absolute(Path::new(value), cwd)),
            None => self
                .home
                .as_deref()
                .map(|home| absolute(home, cwd).join(dot_dir)),
        }
    }
}

/// What a resolution starts from.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// `--config`.
    pub cli_config: Option<PathBuf>,
    /// `--claude-home`.
    pub cli_claude_home: Option<PathBuf>,
    /// `--codex-home`.
    pub cli_codex_home: Option<PathBuf>,
    /// `--db-path` (hidden: only a parent banto passes it).
    pub cli_db_path: Option<PathBuf>,
    /// `--data-dir` (hidden): the data directory the parent resolved.
    pub cli_data_dir: Option<PathBuf>,
    /// `--parent-config` (hidden): the config file the parent loaded.
    pub parent_config: Option<PathBuf>,
    /// `--parent-no-config` (hidden): the parent loaded no config file.
    pub parent_no_config: bool,
    pub env: Env,
    /// The working directory relative paths are taken against; `None` when it
    /// cannot be read, in which case relative paths stay as given.
    pub cwd: Option<PathBuf>,
}

/// Which file a config was loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFile {
    /// No file: the defaults, because the platform has no config location.
    None,
    /// This file (which may not exist, in which case it too meant defaults).
    File(PathBuf),
}

/// The locations a process settled on. Each is absolute, except an empty path
/// and any path resolved without a readable working directory (see the module
/// doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub config_file: ConfigFile,
    /// `None` only when no database location can be determined at all.
    pub db_path: Option<PathBuf>,
    /// banto's own data directory: the one place every working file banto keeps
    /// outside the database is derived from (see [`DataDir`]). `None` when the
    /// platform has none — or when this is a child handed over without one,
    /// because its parent had none.
    pub data_dir: Option<PathBuf>,
    /// `None` only when no Claude home can be determined at all.
    pub claude_home: Option<PathBuf>,
    /// `None` when no Codex home can be determined: then there are no Codex
    /// sessions, which is not an error.
    pub codex_home: Option<PathBuf>,
}

impl Resolved {
    pub fn claude_home(&self) -> Result<ClaudeHome> {
        self.claude_home
            .clone()
            .map(ClaudeHome::new)
            .context("could not determine the Claude home directory; pass --claude-home <PATH>")
    }

    pub fn codex_home(&self) -> Option<CodexHome> {
        self.codex_home.clone().map(CodexHome::new)
    }

    /// The data directory, from which the MCP configs, the consultation requests
    /// and the message logs are all derived.
    pub fn data_dir(&self) -> Option<DataDir> {
        self.data_dir.clone().map(DataDir::new)
    }

    /// The data directory this process may sweep: its own, and only if the
    /// database this process opens *is* that directory's database. A banto
    /// pointed at another database (`db_path` in a config, a test profile) shares
    /// the data directory with the everyday one but not its store, and the store
    /// is the sweep's whole authority — it would call the other one's live files
    /// ownerless. `None` when either location is unknown or not absolute.
    pub fn sweepable_data_dir(&self) -> Option<DataDir> {
        let dir = self.data_dir()?;
        dir.is_database(self.db_path.as_deref()?).then_some(dir)
    }

    /// The database to open. An empty path is an error here, before anything is
    /// opened or created: sqlite would take it for an anonymous temporary
    /// database and the session would silently not persist. Only an empty path
    /// is refused; a whitespace-only one is a (legal) file name, as every other
    /// path key in the config is taken as written.
    pub fn db_path(&self) -> Result<&Path> {
        let path = self
            .db_path
            .as_deref()
            .context("could not determine banto's database path")?;
        if path.as_os_str().is_empty() {
            let from = match &self.config_file {
                ConfigFile::File(file) => format!(" in {}", file.display()),
                ConfigFile::None => String::new(),
            };
            anyhow::bail!(
                "the config key `db_path`{from} is empty (or an empty database path was \
                 passed to this banto): remove the key to use the default location, or give a path"
            );
        }
        Ok(path)
    }
}

/// The variables a parent sets for the `_hook` process, which can take no
/// arguments (see `crate::hook`): what [`Resolved::child_env`] writes and
/// [`Inputs::apply_parent_env`] reads.
pub const ENV_PARENT_CONFIG: &str = "BANTO_PARENT_CONFIG";
pub const ENV_PARENT_NO_CONFIG: &str = "BANTO_PARENT_NO_CONFIG";
pub const ENV_PARENT_DB: &str = "BANTO_PARENT_DB";

impl Resolved {
    /// The command-line flags that make a `banto` child resolve what this
    /// process did. Global flags, so they go anywhere after the subcommand name;
    /// every path is absolute and one argument, spaces and backslashes
    /// included. Only what this process actually resolved is passed. A location
    /// it could not determine is not passed, and the child — handed over by the
    /// very flags this returns — does not look for the data directory or the Codex
    /// home itself. It still falls back to its own search for the Claude home, and
    /// takes a `db_path` from the config the parent loaded; a parent that starts a
    /// child has resolved both, so that fallback is only for a child run by hand.
    pub fn child_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        match &self.config_file {
            ConfigFile::File(path) => {
                args.push("--parent-config".to_string());
                args.push(path.display().to_string());
            }
            ConfigFile::None => args.push("--parent-no-config".to_string()),
        }
        for (flag, path) in [
            ("--db-path", &self.db_path),
            ("--data-dir", &self.data_dir),
            ("--claude-home", &self.claude_home),
            ("--codex-home", &self.codex_home),
        ] {
            if let Some(path) = path {
                args.push(flag.to_string());
                args.push(path.display().to_string());
            }
        }
        args
    }

    /// The same facts as environment variables, for the `_hook` process: its
    /// command line is hashed by Codex to decide whether to trust it, so it
    /// cannot carry them (and cannot be quoted, so a path in it would break on a
    /// space), but it inherits the environment of the agent that runs it. Only
    /// the config file and the database: the hook reads no agent home and nothing
    /// in the data directory (it opens the database, which has its own variable),
    /// so the data directory is not handed to it.
    pub fn child_env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        match &self.config_file {
            ConfigFile::File(path) => {
                env.push((ENV_PARENT_CONFIG.to_string(), path.display().to_string()));
            }
            ConfigFile::None => env.push((ENV_PARENT_NO_CONFIG.to_string(), "1".to_string())),
        }
        if let Some(db) = &self.db_path {
            env.push((ENV_PARENT_DB.to_string(), db.display().to_string()));
        }
        env
    }
}

impl Inputs {
    /// Read what a parent set for the `_hook` process (see
    /// [`Resolved::child_env`]) from `get`. Variables that are absent or empty
    /// change nothing, so a hook started without them resolves for itself. If
    /// both `BANTO_PARENT_CONFIG` and `BANTO_PARENT_NO_CONFIG` are set, "no
    /// config" wins (banto never sets both).
    pub fn apply_parent_env(&mut self, get: impl Fn(&str) -> Option<String>) {
        let value = |key: &str| get(key).filter(|value| !value.is_empty());
        if value(ENV_PARENT_NO_CONFIG).is_some() {
            self.parent_no_config = true;
        }
        if let Some(path) = value(ENV_PARENT_CONFIG) {
            self.parent_config = Some(PathBuf::from(path));
        }
        if let Some(path) = value(ENV_PARENT_DB) {
            self.cli_db_path = Some(PathBuf::from(path));
        }
    }
}

/// `path` against `cwd`, unless it already is absolute, empty, or there is no
/// `cwd` to take it against. Not canonicalised: no symlink is followed and
/// nothing has to exist.
fn absolute(path: &Path, cwd: Option<&Path>) -> PathBuf {
    match cwd {
        Some(cwd) if !path.is_absolute() && !path.as_os_str().is_empty() => cwd.join(path),
        _ => path.to_path_buf(),
    }
}

/// How strictly an explicitly named config file is loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    /// A named file that is missing or does not parse is an error.
    Strict,
    /// Any file problem means the defaults (the `_hook` process, which must
    /// never fail the operator's own agent).
    Lenient,
}

/// Resolve the config, then everything the config can override.
pub fn resolve(inputs: &Inputs, strictness: Strictness) -> Result<(Config, Resolved)> {
    let (config, config_file) = resolve_config(inputs, strictness)?;
    let cwd = inputs.cwd.as_deref();
    // A process the parent started was given the parent's answers; where the
    // parent had none of its own (it could determine no Codex home), the child
    // has none either, rather than finding one the parent did not have.
    let handed_over = inputs.parent_config.is_some() || inputs.parent_no_config;
    // The same rule as the Codex home: a handed child without `--data-dir` has
    // none, because its parent had none and passes every directory it has.
    let data_dir = inputs
        .cli_data_dir
        .as_deref()
        .or((!handed_over)
            .then_some(inputs.env.data_dir.as_deref())
            .flatten())
        .map(|path| absolute(path, cwd));
    let db_path = inputs
        .cli_db_path
        .as_deref()
        .or(config.db_path.as_deref())
        .map(|path| absolute(path, cwd))
        .or_else(|| data_dir.clone().map(|dir| DataDir::new(dir).db_path()));
    let claude_home = inputs
        .cli_claude_home
        .as_deref()
        .or(config.claude_home.as_deref())
        .map(|path| absolute(path, cwd))
        .or_else(|| inputs.env.default_home("CLAUDE_CONFIG_DIR", ".claude", cwd));
    let codex_home = inputs
        .cli_codex_home
        .as_deref()
        .or((!handed_over)
            .then_some(config.codex_home.as_deref())
            .flatten())
        .map(|path| absolute(path, cwd))
        .or_else(|| {
            (!handed_over)
                .then(|| inputs.env.default_home("CODEX_HOME", ".codex", cwd))
                .flatten()
        });
    Ok((
        config,
        Resolved {
            config_file,
            db_path,
            data_dir,
            claude_home,
            codex_home,
        },
    ))
}

fn resolve_config(inputs: &Inputs, strictness: Strictness) -> Result<(Config, ConfigFile)> {
    let cwd = inputs.cwd.as_deref();
    let env = &inputs.env;
    // What the parent already settled on wins over any search: its file is loaded
    // as the parent loaded it (leniently — the parent got this far with it), or
    // there is none.
    if inputs.parent_no_config {
        return Ok((Config::default(), ConfigFile::None));
    }
    if let Some(path) = &inputs.parent_config {
        let path = absolute(path, cwd);
        return Ok((io_config::load_or_default(&path), ConfigFile::File(path)));
    }
    let cli_override = inputs.cli_config.as_deref().map(|path| absolute(path, cwd));
    let env_override = env.banto_config.as_deref().map(|path| absolute(path, cwd));
    let xdg = env
        .xdg_config_home
        .as_deref()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| absolute(path, cwd));
    let home = env.home.as_deref().map(|path| absolute(path, cwd));
    let default_config = env
        .default_config
        .as_deref()
        .map(|path| absolute(path, cwd));
    let source = io_config::resolve_config_path(
        cli_override.as_deref(),
        env_override.as_deref(),
        xdg.as_deref(),
        home.as_deref(),
        default_config,
    );
    Ok(match source {
        io_config::ConfigSource::Explicit(path) => {
            let config = match strictness {
                Strictness::Strict => io_config::load_explicit(&path)
                    .with_context(|| format!("failed to load config file {}", path.display()))?,
                Strictness::Lenient => io_config::load_or_default(&path),
            };
            (config, ConfigFile::File(path))
        }
        io_config::ConfigSource::Discovered(path)
        | io_config::ConfigSource::Default(Some(path)) => {
            (io_config::load_or_default(&path), ConfigFile::File(path))
        }
        io_config::ConfigSource::Default(None) => (Config::default(), ConfigFile::None),
    })
}

#[cfg(test)]
mod agreement_tests;

#[cfg(test)]
mod resolution_tests;
