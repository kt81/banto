//! banto binary: a clap CLI whose default action launches the TUI.
//!
//! Claude-home resolution order (highest priority first):
//! 1. the `--claude-home` flag,
//! 2. `Config.claude_home` from banto's own `config.toml`,
//! 3. the provider default (`~/.claude`).
//!
//! Everything under the resolved Claude home is read strictly read-only.
//! banto's own database (session <-> pane map, groups, pins) lives under
//! `Config.db_path`, falling back to [`config::default_db_path`].
//!
//! `config.toml` itself is located by [`config::resolve_config_path`] (see
//! [`load_config`]): `--config`, then `BANTO_CONFIG`, then
//! `$XDG_CONFIG_HOME/banto/config.toml`, then `~/.config/banto/config.toml`,
//! then the platform default.

mod embedded;
mod mcp;
mod opener;
mod session;
mod sgr;
mod tui;
mod wrap;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use banto_core::config::Config;
use banto_core::status::AgeThresholds;
use banto_io::claude_home::ClaudeHome;
use banto_io::config;
use banto_io::opener::SystemCommandRunner;
use banto_io::process::SystemProcessRunner;
use banto_io::provider::claude_code::ClaudeCodeProvider;
use banto_io::store::Store;

use session::{activity_tag, load_rows, thresholds_from};

/// Search and resume local Claude Code sessions.
#[derive(Parser)]
#[command(name = "banto", version, about, long_about = None)]
struct Cli {
    /// Override the Claude home directory (default: config, else ~/.claude).
    /// Read-only: banto never writes under this path.
    #[arg(long, global = true, value_name = "PATH")]
    claude_home: Option<PathBuf>,

    /// Explicit path to banto's own config.toml. Takes priority over
    /// $BANTO_CONFIG and the XDG/~/.config/platform-default search — and
    /// unlike those, the file must exist and parse (a bad path is a
    /// startup error, not a silent fallback to defaults).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Open the 大店 (emporium) mode: banto as a persistent sidebar plus an
    /// embedded session pane, instead of the default 帳場 (chōba) list.
    #[arg(long, visible_alias = "oodana")]
    emporium: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print all sessions as plain text (newest first), one per line.
    List,
    /// Internal: supervises a resumed or brand-new session's process. The
    /// opener spawns this; it is not meant to be invoked directly.
    #[command(name = "_wrap", hide = true)]
    Wrap {
        /// The session id being resumed (required unless `--new-session`).
        #[arg(long, required_unless_present = "new_session")]
        session: Option<String>,
        /// New-session mode: there is no known session id yet — discover
        /// the one Claude assigns to `--cwd` and track it so it can later
        /// be focused instead of double-resumed.
        #[arg(long, requires = "cwd", conflicts_with = "session")]
        new_session: bool,
        /// The new session's launch directory (new-session mode only).
        #[arg(long, requires = "new_session")]
        cwd: Option<PathBuf>,
        /// Which backend this pane was opened with (new-session mode
        /// only) — `psmux` or `windows-terminal`; the *opener*'s own
        /// already-resolved choice, passed through explicitly rather than
        /// re-detected from the environment (e.g. `$TMUX_PANE` could be
        /// inherited even when banto is configured to open new sessions
        /// via Windows Terminal, if banto itself happens to be running
        /// inside an unrelated tmux session).
        #[arg(long, requires = "new_session", required_if_eq("new_session", "true"))]
        backend: Option<String>,
        /// Diagnostic log path for this process's own `WrapLog` (new-session
        /// mode only), passed explicitly by the opener rather than left for
        /// this process to read `BANTO_WRAP_LOG` from its own environment: a
        /// psmux-spawned process does not reliably inherit banto's
        /// environment (docs/notes/psmux-spike.md) — see
        /// `crate::wrap::WrapLog::new`.
        #[arg(long, requires = "new_session")]
        wrap_log: Option<PathBuf>,
        /// The command to run, e.g. `claude --resume <id>` (resume mode)
        /// or plain `claude` (new-session mode).
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Internal/dev: host a command inside banto's own TUI as an embedded
    /// terminal (single pane). Exercises the embedded-multiplexer path.
    #[command(name = "_embed", hide = true)]
    Embed {
        /// Working directory for the hosted command (default: inherit banto's).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// The command to host, e.g. `-- claude --resume <id>`.
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Internal: banto's MCP server on stdio (the brigade Director<->Worker
    /// mediation channel). Spawned by an embedded `claude` via `--mcp-config`;
    /// not meant to be invoked directly.
    #[command(name = "_mcp", hide = true)]
    Mcp {
        /// The calling session's id (echoed by the ping tool; the fallback
        /// used to resolve membership when `--member` is absent, for
        /// `--mcp-config` files written before it existed).
        #[arg(long)]
        session: Option<String>,
        /// The brigade this session belongs to — enables the message tools.
        #[arg(long)]
        brigade: Option<i64>,
        /// This session's banto-owned member token within the brigade
        /// (`director`, `worker-1`, `worker-2`, ...).
        #[arg(long)]
        member: Option<String>,
        /// This session's role in the brigade: `director` or `worker`.
        /// Parsed for compatibility with `--mcp-config` files already on
        /// disk; the live role always comes from the store instead.
        #[arg(long)]
        role: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = load_config(cli.config.as_deref())?;

    match cli.command {
        Some(Command::List) => {
            let claude_home = resolve_claude_home(cli.claude_home, &config)?;
            let thresholds = thresholds_from(&config.activity);
            run_list(&claude_home, &thresholds)
        }
        Some(Command::Wrap {
            session,
            new_session,
            cwd,
            backend,
            wrap_log,
            argv,
        }) => {
            let store = open_store(&config)?;
            let code = if new_session {
                let cwd = cwd.expect("clap requires --cwd with --new-session");
                let backend_key = backend.expect("clap requires --backend with --new-session");
                let backend = opener::parse_backend_key(&backend_key)
                    .with_context(|| format!("unknown --backend value: {backend_key}"))?;
                let claude_home = resolve_claude_home(cli.claude_home, &config)?;
                let provider = ClaudeCodeProvider::new(claude_home);
                wrap::run_new_session(
                    &store,
                    &cwd,
                    &argv,
                    backend,
                    wrap_log.as_deref(),
                    |key| std::env::var(key).ok(),
                    wrap::NewSessionDeps {
                        process_runner: &SystemProcessRunner,
                        command_runner: &SystemCommandRunner,
                        provider: &provider,
                    },
                )?
            } else {
                let session = session.expect("clap requires --session unless --new-session");
                wrap::run(&store, &session, &argv, &SystemProcessRunner)?
            };
            std::process::exit(code)
        }
        Some(Command::Embed { cwd, argv }) => embedded::run_embedded(&argv, cwd.as_deref()),
        Some(Command::Mcp {
            session,
            brigade,
            member,
            role,
        }) => {
            let store = open_store(&config)?;
            let claude_home = resolve_claude_home(cli.claude_home, &config)?;
            let identity = mcp::Identity {
                session,
                brigade,
                member,
                role: role.as_deref().and_then(mcp::parse_role),
            };
            mcp::run_stdio_server(store, identity, claude_home)
        }
        None => {
            let claude_home = resolve_claude_home(cli.claude_home, &config)?;
            let thresholds = thresholds_from(&config.activity);
            // `Store::set_session_group` (the `g` modal) takes `&mut self`
            // (it wraps a transaction), and the store is shared by both the
            // chōba TUI and emporium, so a `RefCell` gives interior
            // mutability without threading `&mut Store` through every handler.
            let store = std::cell::RefCell::new(open_store(&config)?);
            if cli.emporium {
                // The 大店 (emporium) mode: a separate top-level TUI chosen at
                // launch (`--emporium` / `--oodana`).
                embedded::run_emporium(
                    &claude_home,
                    &thresholds,
                    &store,
                    &config.brigade,
                    &config.keys,
                )
            } else {
                tui::run(&claude_home, &thresholds, config.opener, &store)
            }
        }
    }
}

/// Load banto's own config via [`config::resolve_config_path`]'s resolution
/// order. An explicit override (`--config` / `BANTO_CONFIG`) must exist and
/// parse, or startup fails with a plain one-line error; every other tier
/// (XDG/`~/.config` discovery, the platform default) is lenient — a missing
/// or broken file there just means defaults, same as before this round.
fn load_config(cli_override: Option<&Path>) -> Result<Config> {
    let env_override = std::env::var_os("BANTO_CONFIG").map(PathBuf::from);
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = dirs::home_dir();

    let source = config::resolve_config_path(
        cli_override,
        env_override.as_deref(),
        xdg_config_home.as_deref(),
        home.as_deref(),
    );

    match source {
        config::ConfigSource::Explicit(path) => config::load_explicit(&path)
            .with_context(|| format!("failed to load config file {}", path.display())),
        config::ConfigSource::Discovered(path) => Ok(config::load_or_default(&path)),
        config::ConfigSource::Default(Some(path)) => Ok(config::load_or_default(&path)),
        config::ConfigSource::Default(None) => Ok(Config::default()),
    }
}

/// Resolve the Claude home directory per the documented priority order.
fn resolve_claude_home(flag: Option<PathBuf>, config: &Config) -> Result<ClaudeHome> {
    flag.or_else(|| config.claude_home.clone())
        .map(ClaudeHome::new)
        .or_else(ClaudeHome::default_home)
        .context("could not determine the Claude home directory; pass --claude-home <PATH>")
}

/// Open (creating if needed) banto's own sqlite database at
/// `Config.db_path`, falling back to [`config::default_db_path`].
fn open_store(config: &Config) -> Result<Store> {
    let path = config
        .db_path
        .clone()
        .or_else(config::default_db_path)
        .context("could not determine banto's database path")?;
    Store::open(&path).context("failed to open banto's database")
}

/// `banto list`: one line per session — activity tag, id, title, cwd.
fn run_list(claude_home: &ClaudeHome, thresholds: &AgeThresholds) -> Result<()> {
    let rows = load_rows(claude_home, thresholds).context("failed to read sessions")?;
    for row in &rows {
        let title = row.title.as_deref().unwrap_or("(no title)");
        let cwd = row
            .cwd
            .as_ref()
            .map_or_else(|| "-".to_string(), |path| path.display().to_string());
        println!(
            "{:<5}  {}  {}  {}",
            activity_tag(row.activity),
            row.id,
            title,
            cwd
        );
    }
    Ok(())
}
