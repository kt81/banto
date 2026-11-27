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
//! The data directory has four sources, in this order: the hidden `--data-dir`
//! flag (what a parent hands down), `BANTO_DATA_DIR`, the config's `data_dir`,
//! the platform's `<data_local>/banto`. It is refused when empty or inside an
//! agent's home (invariant 1: those are read-only for banto), and so is a
//! database path ([`Resolved::db_path`]); [`resolved_place`] and [`is_within`]
//! say what "inside" compares.
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
use banto_core::model::AgentKind;
use banto_io::claude_home::ClaudeHome;
use banto_io::codex_home::CodexHome;
use banto_io::config as io_config;
use banto_io::data_dir::DataDir;
use banto_io::file_identity::directory_identity;

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
    /// `BANTO_DATA_DIR`. Empty means unset; that is decided in [`resolve`], so a
    /// test can say it.
    pub banto_data_dir: Option<PathBuf>,
    /// The platform's data directory for banto (`<data_local>/banto`): where the
    /// database lives by default, and where banto keeps its MCP configs,
    /// consultation requests and message logs whatever the database path is.
    pub data_dir: Option<PathBuf>,
    /// `TERM_PROGRAM`, which [`banto_core::config::resolve_host_notify`] reads
    /// for `[attention] notify = "auto"`. Unset and not-UTF-8 are both `None`.
    pub term_program: Option<String>,
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
            banto_data_dir: std::env::var_os("BANTO_DATA_DIR").map(PathBuf::from),
            default_config: io_config::default_config_path(),
            data_dir: DataDir::default_root(),
            term_program: std::env::var("TERM_PROGRAM").ok(),
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

    /// The home an agent started in a pane settles on, from this environment
    /// alone: the rule of [`Self::default_home`], except that the variable is
    /// kept as written (see [`AgentHome::Variable`]).
    fn agent_home(&self, variable: &str, dot_dir: &str, cwd: Option<&Path>) -> Option<AgentHome> {
        let from_env = match variable {
            "CLAUDE_CONFIG_DIR" => &self.claude_config_dir,
            _ => &self.codex_home,
        };
        match from_env.as_deref().filter(|value| !value.is_empty()) {
            Some(value) => Some(AgentHome::Variable(value.to_string())),
            None => self
                .home
                .as_deref()
                .map(|home| AgentHome::Default(absolute(home, cwd).join(dot_dir))),
        }
    }
}

/// The hook waiter is handed an absolute database path and loads no config.
pub fn wake_db_path_safe(path: &Path) -> bool {
    if !path.is_absolute() || path.as_os_str().is_empty() {
        return false;
    }
    let env = Env::from_process();
    let cwd = std::env::current_dir().ok();
    let claude = env.default_home("CLAUDE_CONFIG_DIR", ".claude", cwd.as_deref());
    let codex = env.default_home("CODEX_HOME", ".codex", cwd.as_deref());
    inside_an_agent_home(path, claude.as_deref(), codex.as_deref()).is_none()
}

/// Where an agent in a pane keeps its home. banto sets neither agent's variable
/// and passes no flag, so the pane's agent has only these two to go by.
#[derive(Debug, PartialEq, Eq)]
enum AgentHome {
    /// The product's variable as written. A relative one is not made absolute
    /// here: banto would read it against its own working directory, and the
    /// agent against the pane's, which is the session's and not banto's.
    Variable(String),
    /// `<home>/<dot dir>`, the platform default.
    Default(PathBuf),
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
        self.claude_home_in(|key| std::env::var(key).ok())
    }

    /// [`Self::claude_home`] over an injected environment.
    ///
    /// The root is what banto lists (`--claude-home` and the config key win
    /// over the variable), but the `.claude.json` that decides trust is the
    /// one the agent in a pane reads, and a pane's agent is started from
    /// banto's own environment with no `--claude-home` of its own. So that file
    /// is named from the environment alone, whatever the root: it is what the
    /// agent will consult, and an advisory about the agent has nothing else to
    /// read.
    fn claude_home_in(&self, env: impl Fn(&str) -> Option<String>) -> Result<ClaudeHome> {
        let home =
            self.claude_home.clone().map(ClaudeHome::new).context(
                "could not determine the Claude home directory; pass --claude-home <PATH>",
            )?;
        Ok(match ClaudeHome::agent_trust_registry_from_env(env) {
            Some(registry) => home.with_trust_registry(registry),
            None => home,
        })
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
    /// path key in the config is taken as written. A path inside an agent's home
    /// is refused too: opening it would create a file there. Resolution itself
    /// carries on past both (`list` and `versions` open no store); this is the
    /// one door every command that does open it goes through, the hook included.
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
        if !path.is_absolute() {
            anyhow::bail!(
                "the database path {} is relative and the working directory could not be \
                 read to make it absolute: give an absolute path",
                path.display()
            );
        }
        if let Some((label, home)) = inside_an_agent_home(
            path,
            self.claude_home.as_deref(),
            self.codex_home.as_deref(),
        ) {
            let from = match &self.config_file {
                ConfigFile::File(file) => format!(" in {}", file.display()),
                ConfigFile::None => String::new(),
            };
            anyhow::bail!(
                "the database path {} (the config key `db_path`{from}, or a path passed to this \
                 banto) is inside the {label} {}: banto never writes inside an agent's home. \
                 Remove the key to use the default location, or give a path outside it",
                path.display(),
                home.display()
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
pub const ENV_PARENT_DATA_DIR: &str = "BANTO_PARENT_DATA_DIR";

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
    /// space), but it inherits the environment of the agent that runs it. The
    /// config file, the database and the data directory: the hook resolves no
    /// agent home, and the data directory is only for the one file it reads from
    /// there, a Goinkyo's consultation request. The session's rollout, the other
    /// file it reads, is named by Codex on the hook's stdin.
    ///
    /// These reach the environment of every brigade member's pane, Claude ones
    /// included (`embedded::brigade_env` sets them for any member), and only `_hook`
    /// reads them.
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
        if let Some(dir) = &self.data_dir {
            env.push((ENV_PARENT_DATA_DIR.to_string(), dir.display().to_string()));
        }
        env
    }
}

impl Inputs {
    /// Whether a parent banto handed this process its answers. A handed process
    /// finds nothing for itself that its parent did not pass.
    fn handed_over(&self) -> bool {
        self.parent_config.is_some() || self.parent_no_config
    }

    /// Read what a parent set for the `_hook` process (see
    /// [`Resolved::child_env`]) from `get`. Variables that are absent or empty
    /// change nothing, so a hook started without them resolves for itself. If
    /// both `BANTO_PARENT_CONFIG` and `BANTO_PARENT_NO_CONFIG` are set, "no
    /// config" wins (banto never sets both). The data directory is read as the
    /// `--data-dir` flag a handed-over process takes (`data_dir_source`).
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
        if let Some(path) = value(ENV_PARENT_DATA_DIR) {
            self.cli_data_dir = Some(PathBuf::from(path));
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

/// How strictly a config file is loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    /// A file that is there must be usable: one that cannot be read or parsed is
    /// an error, whichever way it was found. A missing file is the defaults,
    /// except for an explicit `--config`/`BANTO_CONFIG`, where it is an error
    /// too (the operator named it).
    Strict,
    /// Any file problem means the defaults (the `_hook` process, which must
    /// never fail the operator's own agent).
    Lenient,
}

/// What the operator is told when banto stops on a config file it cannot use.
/// Built by hand rather than from `ConfigError`'s `Display`, which spells the
/// path with `{:?}` (every backslash of a Windows path doubled) and would put
/// it inside an `anyhow` "Caused by" chain; `toml`'s own text for a syntax
/// error (line, column, the offending line, what was expected) is the part
/// worth keeping verbatim.
pub fn refusal_message(error: &io_config::ConfigError) -> String {
    format!(
        "banto: config file {} could not be used:\n{}\nbanto did not start. Fix the file and run banto again.",
        error.path().display(),
        error.reason()
    )
}

/// What a `_mcp` process says through its refusing server for the same
/// condition. Its reader is an agent in a pane, not an operator at a prompt,
/// and the process cannot stop the way the UI does: exiting would show the
/// agent only an MCP server that failed to start, with none of these words.
pub fn mcp_refusal_message(error: &io_config::ConfigError) -> String {
    format!(
        "banto's config file {} could not be used:\n{}\nFix the file, then reconnect banto's MCP server (Claude: /mcp) or restart the member.",
        error.path().display(),
        error.reason()
    )
}

/// Resolve the config, then everything the config can override.
pub fn resolve(inputs: &Inputs, strictness: Strictness) -> Result<(Config, Resolved)> {
    let (config, config_file) = resolve_config(inputs, strictness)?;
    let cwd = inputs.cwd.as_deref();
    // A process the parent started was given the parent's answers; where the
    // parent had none of its own (it could determine no Codex home), the child
    // has none either, rather than finding one the parent did not have.
    let handed_over = inputs.handed_over();
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
    // The hook must never fail the operator's own agent over banto's
    // configuration, and reads the data directory only to name a Goinkyo's
    // consultation request: where this one is refused it goes without, and the
    // briefing then has no request path.
    let data_dir = match checked_data_dir(
        inputs,
        &config,
        &config_file,
        claude_home.as_deref(),
        codex_home.as_deref(),
    ) {
        Ok(dir) => dir,
        Err(err) if strictness == Strictness::Strict => return Err(err),
        Err(_) => None,
    };
    let db_path = inputs
        .cli_db_path
        .as_deref()
        .or(config.db_path.as_deref())
        .map(|path| absolute(path, cwd))
        .or_else(|| data_dir.clone().map(|dir| DataDir::new(dir).db_path()));
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

/// Where the data directory came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataDirSource {
    /// `--data-dir`: the operator's own, or what a parent passed down.
    Flag,
    /// `BANTO_DATA_DIR`.
    Variable,
    /// The top-level `data_dir` key of the config.
    Config,
    /// `<data_local>/banto`.
    Platform,
}

impl DataDirSource {
    /// The words an error uses for it.
    fn describe(self, config_file: &ConfigFile) -> String {
        match self {
            Self::Flag => "`--data-dir`".to_string(),
            Self::Variable => "`BANTO_DATA_DIR`".to_string(),
            Self::Config => match config_file {
                ConfigFile::File(file) => {
                    format!("the config key `data_dir` in {}", file.display())
                }
                ConfigFile::None => "the config key `data_dir`".to_string(),
            },
            Self::Platform => "the platform's default data directory".to_string(),
        }
    }
}

/// The winning source of the data directory and what it said, as written. An
/// empty `BANTO_DATA_DIR` is no source at all; an empty flag or config value
/// still is one, so that it can be refused by name rather than be mistaken for
/// the default. A handed-over process takes the flag alone: its parent resolved
/// the directory and passes it, so a variable or config of its own is no second
/// opinion (the Codex home follows the same rule).
fn data_dir_source<'a>(
    inputs: &'a Inputs,
    config: &'a Config,
) -> Option<(DataDirSource, &'a Path)> {
    if let Some(flag) = inputs.cli_data_dir.as_deref() {
        return Some((DataDirSource::Flag, flag));
    }
    if inputs.handed_over() {
        return None;
    }
    inputs
        .env
        .banto_data_dir
        .as_deref()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| (DataDirSource::Variable, path))
        .or_else(|| {
            config
                .data_dir
                .as_deref()
                .map(|path| (DataDirSource::Config, path))
        })
        .or_else(|| {
            inputs
                .env
                .data_dir
                .as_deref()
                .map(|path| (DataDirSource::Platform, path))
        })
}

/// The data directory, absolute, or why it cannot be used: empty, or inside an
/// agent's home. Not required to exist.
fn checked_data_dir(
    inputs: &Inputs,
    config: &Config,
    config_file: &ConfigFile,
    claude_home: Option<&Path>,
    codex_home: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let Some((source, written)) = data_dir_source(inputs, config) else {
        return Ok(None);
    };
    let from = source.describe(config_file);
    if written.as_os_str().is_empty() {
        anyhow::bail!("{from} is empty: remove it to use the default location, or give a path");
    }
    let dir = absolute(written, inputs.cwd.as_deref());
    if !dir.is_absolute() {
        anyhow::bail!(
            "{from} is {}, a relative path, and the working directory could not be read to \
             make it absolute: give an absolute path",
            dir.display()
        );
    }
    if let Some((label, home)) = inside_an_agent_home(&dir, claude_home, codex_home) {
        anyhow::bail!(
            "{from} is {}, which is inside the {label} {}: banto never writes inside an \
             agent's home. Name a directory outside it",
            dir.display(),
            home.display()
        );
    }
    Ok(Some(dir))
}

/// The status-line notice for a database that is not in the data directory the
/// operator chose, or `None`. Only when both were named — the directory by the
/// flag, the variable or the config (not the platform default), the database
/// by `db_path` — and they disagree. `db_path` alone is the long-standing way
/// to point at another database and stays silent. The consequence it states is
/// real: the sweep only runs on the database that is `<data dir>/banto.db`
/// ([`Resolved::sweepable_data_dir`]), while the MCP configs, consultation
/// requests and message logs follow the directory whatever the database is.
pub fn split_database_notice(
    inputs: &Inputs,
    config: &Config,
    resolved: &Resolved,
) -> Option<String> {
    let (source, _) = data_dir_source(inputs, config)?;
    if source == DataDirSource::Platform {
        return None;
    }
    let dir = resolved.data_dir()?;
    (resolved.db_path.as_deref()? != dir.db_path()).then(|| {
        "data_dir: the database is not in it, so the data-directory sweep does not run; \
         MCP, request and log files stay in the data directory"
            .to_string()
    })
}

/// One product's side of [`agent_home_notices`].
struct HomeKind {
    /// How the notice names the product.
    label: &'static str,
    /// The environment variable the product's own home follows.
    variable: &'static str,
    /// The directory under the user's home when the variable is not set.
    dot_dir: &'static str,
}

const CLAUDE_HOME: HomeKind = HomeKind {
    label: "Claude",
    variable: "CLAUDE_CONFIG_DIR",
    dot_dir: ".claude",
};

const CODEX_HOME: HomeKind = HomeKind {
    label: "Codex",
    variable: "CODEX_HOME",
    dot_dir: ".codex",
};

/// The status-line notices for a home banto lists that is not the home the agent
/// it opens will use: one per enabled product at most.
///
/// banto resolves a home as flag, then config key, then the variable, then the
/// default; an agent in a pane is started from banto's environment and told none
/// of that (`docs/REQUIREMENTS.md`: the agents' own variables are not banto's to
/// set), so it has the variable or the default. They part when a flag or a config
/// key names another place, and nothing fails until a pane opens: banto lists one
/// home's sessions and resumes them in an agent reading another.
///
/// A relative variable is reported whatever it resolves to for banto, which makes
/// it absolute against its own working directory ([`AgentHome::Variable`]).
pub fn agent_home_notices(inputs: &Inputs, config: &Config, resolved: &Resolved) -> Vec<String> {
    let enabled = banto_core::config::resolve_agents(&config.agents).enabled;
    [
        (AgentKind::ClaudeCode, CLAUDE_HOME, &resolved.claude_home),
        (AgentKind::Codex, CODEX_HOME, &resolved.codex_home),
    ]
    .into_iter()
    .filter(|(product, _, _)| enabled.contains(product))
    .filter_map(|(_, kind, listed)| {
        let listed = listed.as_deref()?;
        let agent = inputs
            .env
            .agent_home(kind.variable, kind.dot_dir, inputs.cwd.as_deref())?;
        agent_home_notice(&kind, listed, &agent)
    })
    .collect()
}

fn agent_home_notice(kind: &HomeKind, listed: &Path, agent: &AgentHome) -> Option<String> {
    let HomeKind {
        label, variable, ..
    } = kind;
    let (used, from) = match agent {
        AgentHome::Variable(value) if !Path::new(value).is_absolute() => {
            return Some(format!(
                "{label} home: banto lists {label} sessions under {}, but {variable} is the \
                 relative path {value}, which each pane's working directory can resolve differently",
                listed.display()
            ));
        }
        AgentHome::Variable(value) => (PathBuf::from(value), format!("from {variable}")),
        AgentHome::Default(path) => (path.clone(), "the default".to_string()),
    };
    (!same_place(listed, &used)).then(|| {
        format!(
            "{label} home: banto lists {label} sessions under {}, but a {label} opened here \
             uses {} ({from})",
            listed.display(),
            used.display()
        )
    })
}

/// Whether two paths are one directory: the same place by spelling once
/// [`resolved_place`] has asked the filesystem, or the very same directory under
/// two names (`banto_io::file_identity`). The two questions
/// [`inside_an_agent_home`] asks, for equality rather than containment.
fn same_place(a: &Path, b: &Path) -> bool {
    let rules = Rules::of_this_platform();
    let (place_a, place_b) = (resolved_place(a), resolved_place(b));
    (is_within(&place_a, &place_b, rules) && is_within(&place_b, &place_a, rules))
        || directory_identity(a)
            .ok()
            .zip(directory_identity(b).ok())
            .is_some_and(|(one, other)| one == other)
}

/// What startup has to say to the operator before the first frame, in the order
/// the status line carries it: the database that is not where the data directory
/// is ([`split_database_notice`]), the homes banto lists that an agent opened in
/// a pane will not use ([`agent_home_notices`]), then `instance_notice`
/// (`single_instance`'s, about the check for another banto on the database), then
/// the config's legacy `[brigade]` keys. Pulled out of `main` so that a notice
/// dropped from the line is a failing test.
pub fn boot_notices(
    inputs: &Inputs,
    config: &Config,
    resolved: &Resolved,
    instance_notice: Option<String>,
) -> Vec<String> {
    split_database_notice(inputs, config, resolved)
        .into_iter()
        .chain(agent_home_notices(inputs, config, resolved))
        .chain(instance_notice)
        .chain(config.brigade.legacy_key_notice())
        .collect()
}

/// The agent home that `path` is, or lies inside, with its name for an error.
/// Only the homes this process resolved are known.
///
/// Two questions, and either answering yes is a yes. By spelling: both sides go
/// through [`resolved_place`] and are compared as names, which is what sees a
/// home that is not there yet. By identity: some existing ancestor of `path` (the
/// path itself included) is the very directory a home is, whatever either was
/// called to reach it. Spelling alone cannot be complete, because Windows gives
/// one directory unboundedly many names (`\\localhost\C$\…`, a computer name,
/// an address); see `banto_io::file_identity`. An ancestor or home whose identity
/// cannot be read — it does not exist, or may not be opened — is not decided by
/// identity, and the spelling answer stands.
fn inside_an_agent_home<'a>(
    path: &Path,
    claude_home: Option<&'a Path>,
    codex_home: Option<&'a Path>,
) -> Option<(&'static str, &'a Path)> {
    let rules = Rules::of_this_platform();
    let place = resolved_place(path);
    let ancestors: Vec<_> = path
        .ancestors()
        .filter_map(|ancestor| directory_identity(ancestor).ok())
        .collect();
    [("Claude home", claude_home), ("Codex home", codex_home)]
        .into_iter()
        .find_map(|(label, home)| {
            let home = home?;
            let same_place = is_within(&place, &resolved_place(home), rules);
            let same_directory =
                directory_identity(home).is_ok_and(|identity| ancestors.contains(&identity));
            (same_place || same_directory).then_some((label, home))
        })
}

/// `path` as the filesystem names it, without creating anything: the longest
/// ancestor that exists goes through `canonicalize`, which follows symlinks and
/// junctions and, on Windows, turns an 8.3 short name (`CLAUDE~1`) into the long
/// one and drops a trailing dot or space; what is left below it is appended as
/// written, for [`normal_parts`] to read. A candidate that is not there yet,
/// which is the usual case for a data directory, is judged by where it would
/// be made. A home gets the same treatment, so the two sides always agree on
/// what their common ancestors are (macOS's `/var` is `/private/var` for both).
///
/// Not airtight. It is a check made once at startup, so a link made afterwards
/// is not seen, and a dangling link is a name like any other here — one that is
/// made good later leads somewhere this did not look.
fn resolved_place(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut probe = path.to_path_buf();
    loop {
        if let Ok(real) = probe.canonicalize() {
            let mut place = real;
            place.extend(tail.iter().rev());
            return place;
        }
        match (probe.components().next_back(), probe.parent()) {
            (Some(last), Some(parent)) => {
                tail.push(last.as_os_str().to_os_string());
                probe = parent.to_path_buf();
            }
            // Nothing of it exists, down to the root: it is what it says.
            _ => return path.to_path_buf(),
        }
    }
}

/// How two spellings of a path are told to be one place. Only what the path
/// alone can say: [`resolved_place`] is what asks the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rules {
    /// Names differing only in case are the same. True where the default volume
    /// is case-insensitive (NTFS, APFS). On a volume that is not this
    /// over-refuses, which is the safe direction for a guard.
    fold_case: bool,
    /// Win32's reading of a name: a trailing dot or space is dropped
    /// (`.claude.` and `.claude ` are `.claude`) and so is anything from a
    /// colon on (`.claude::$INDEX_ALLOCATION` is the directory itself).
    win32_names: bool,
}

impl Rules {
    fn of_this_platform() -> Self {
        Self {
            fold_case: cfg!(any(windows, target_os = "macos")),
            win32_names: cfg!(windows),
        }
    }

    /// Every spelling distinct.
    #[cfg(test)]
    const EXACT: Self = Self {
        fold_case: false,
        win32_names: false,
    };

    /// One name as these rules read it, or `None` when nothing is left of it.
    fn name(self, text: &str) -> Option<String> {
        let mut name = text;
        if self.win32_names {
            name = name.split(':').next().unwrap_or("");
            name = name.trim_end_matches(['.', ' ']);
        }
        if name.is_empty() {
            return None;
        }
        Some(if self.fold_case {
            name.to_lowercase()
        } else {
            name.to_string()
        })
    }
}

/// One step of a path, with the spelling differences that do not name another
/// place taken out.
#[derive(Debug, PartialEq, Eq)]
enum Part {
    /// A drive, a share or the root: where the path starts.
    Anchor(String),
    Name(String),
}

/// `path` as a list of [`Part`]s: separators unified, `.` dropped, `..` applied
/// lexically, a verbatim drive (`\\?\C:`) read as the drive and a verbatim share
/// (`\\?\UNC\server\share`) as the plain one, and every name read by `rules`.
/// Nothing is read from the disk.
fn normal_parts(path: &Path, rules: Rules) -> Vec<Part> {
    use std::path::{Component, Prefix};
    let anchor = |text: String| Part::Anchor(rules.name(&text).unwrap_or(text));
    let mut parts: Vec<Part> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => parts.push(match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                    // Not through `name`: a drive's colon is its own.
                    let drive = format!("{}:", char::from(letter));
                    Part::Anchor(if rules.fold_case {
                        drive.to_lowercase()
                    } else {
                        drive
                    })
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    let share = format!(
                        r"\\{}\{}",
                        server.to_string_lossy(),
                        share.to_string_lossy()
                    );
                    Part::Anchor(if rules.fold_case {
                        share.to_lowercase()
                    } else {
                        share
                    })
                }
                _ => anchor(prefix.as_os_str().to_string_lossy().into_owned()),
            }),
            Component::RootDir => parts.push(Part::Anchor("/".to_string())),
            Component::CurDir => {}
            Component::ParentDir => match parts.last() {
                Some(Part::Name(name)) if name != ".." => {
                    parts.pop();
                }
                // `..` at the root is the root.
                Some(Part::Anchor(_)) => {}
                _ => parts.push(Part::Name("..".to_string())),
            },
            Component::Normal(name) => {
                if let Some(name) = rules.name(&name.to_string_lossy()) {
                    parts.push(Part::Name(name));
                }
            }
        }
    }
    parts
}

/// Whether `path` is `dir` or lies below it, compared as normalised paths
/// ([`normal_parts`]): by spelling alone, which is why [`inside_an_agent_home`]
/// asks the filesystem first ([`resolved_place`]). The rules are an argument so every answer can be
/// tested on any platform. An empty `dir` contains nothing.
fn is_within(path: &Path, dir: &Path, rules: Rules) -> bool {
    let dir = normal_parts(dir, rules);
    !dir.is_empty() && normal_parts(path, rules).starts_with(&dir)
}

fn resolve_config(inputs: &Inputs, strictness: Strictness) -> Result<(Config, ConfigFile)> {
    let cwd = inputs.cwd.as_deref();
    let env = &inputs.env;
    // The file at `path` as `strictness` reads one that is not an explicit
    // override: a missing one is the defaults in both, anything else wrong with
    // it only for the hook.
    let load_found = |path: &Path| match strictness {
        Strictness::Strict => io_config::load(path)
            .with_context(|| format!("failed to load config file {}", path.display())),
        Strictness::Lenient => Ok(io_config::load_or_default(path)),
    };
    // What the parent already settled on wins over any search: its file is loaded
    // the way the parent loaded it, or there is none. The parent got this far
    // with it, so a file that is now broken was edited since, and the child says
    // so. A file that is *missing* is the defaults, not an error: the parent
    // names its platform default whether or not one exists (`ConfigFile::File`),
    // and a machine with no config.toml is the ordinary case.
    if inputs.parent_no_config {
        return Ok((Config::default(), ConfigFile::None));
    }
    if let Some(path) = &inputs.parent_config {
        let path = absolute(path, cwd);
        return Ok((load_found(&path)?, ConfigFile::File(path)));
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
            (load_found(&path)?, ConfigFile::File(path))
        }
        io_config::ConfigSource::Default(None) => (Config::default(), ConfigFile::None),
    })
}

#[cfg(test)]
mod agent_home_tests;
#[cfg(test)]
mod agreement_tests;

#[cfg(test)]
mod data_dir_tests;

#[cfg(test)]
mod resolution_tests;
