//! banto binary: a clap CLI whose default action launches the TUI.
//!
//! Both products' homes resolve in the same shape and priority order
//! (highest first), so that a flag and a `config.toml` entry always beat
//! the product's own environment variable rather than one product
//! honouring it unconditionally and the other ignoring it:
//! 1. the `--claude-home` / `--codex-home` flag,
//! 2. `Config.claude_home` / `Config.codex_home` from banto's own
//!    `config.toml` — banto-specific and deliberately set, so it outranks
//!    an ambient environment variable meant for the provider CLI itself,
//! 3. the provider's own override variable (`$CLAUDE_CONFIG_DIR` /
//!    `$CODEX_HOME`),
//! 4. the provider default (`~/.claude` / `~/.codex`).
//!
//! See [`resolve_claude_home`] and [`resolve_codex_home`]. They differ only
//! in failure mode: no Claude home is a startup error (`--claude-home` is
//! the escape hatch), while no Codex home just means no Codex sessions —
//! Codex support is optional, Claude's is not.
//!
//! Everything under the resolved Claude home, and under the resolved Codex
//! home, is treated as strictly read-only.
//! banto's own database (session <-> pane map, groups, pins) lives under
//! `Config.db_path`, falling back to [`config::default_db_path`].
//!
//! `config.toml` itself is located by [`config::resolve_config_path`]'s
//! resolution order (see [`load_config`]).
//!
//! `Config.agents` gates which products get discovered at all — resolved
//! once here via `banto_core::config::resolve_agents` into `resolved_agents`
//! and passed down to every entry point, since `session::discover_all` (not
//! any of these) is what actually decides which provider runs.
//! `ResolvedAgents::ignored` is where a name banto didn't recognize ends up;
//! `tui::run`/`embedded::run_emporium` are the ones that tell the operator
//! about it, once, at startup — see `session::agents_ignored_notice`.

mod briefing;
mod embedded;
mod hook;
mod mcp;
mod opener;
mod session;
mod sgr;
mod tui;
mod wrap;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use banto_core::config::Config;
use banto_core::model::AgentKind;
use banto_core::status::AgeThresholds;
use banto_io::claude_home::ClaudeHome;
use banto_io::codex_home::CodexHome;
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
    /// Override the Claude home directory (default: config, else
    /// $CLAUDE_CONFIG_DIR, else ~/.claude). Read-only: banto never writes
    /// under this path.
    #[arg(long, global = true, value_name = "PATH")]
    claude_home: Option<PathBuf>,

    /// Override the Codex home directory (default: config, else
    /// $CODEX_HOME, else ~/.codex). Read-only: banto never writes under
    /// this path.
    #[arg(long, global = true, value_name = "PATH")]
    codex_home: Option<PathBuf>,

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
    /// Internal: the Codex `SessionStart` hook process (see `crate::hook`'s
    /// module doc). Takes no arguments, deliberately, forever: Codex trusts
    /// a hook by hashing its literal command string, and adding so much as a
    /// flag here would silently break that trust for every brigade launch
    /// site already approved. Member identity travels through
    /// `BANTO_BRIGADE`/`BANTO_MEMBER`/`BANTO_ROLE` in the environment
    /// instead.
    #[command(name = "_hook", hide = true)]
    Hook,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // `Command::Hook` is dispatched before `load_config(...)?` runs at all
    // (and before it can fail): this process is spawned mid-startup of the
    // *operator's own* unrelated Codex session, so a broken --config/
    // BANTO_CONFIG the operator set for some other reason must never take
    // the hook down with it. `hook::run` resolves and loads its own config
    // independently, always leniently.
    if matches!(cli.command, Some(Command::Hook)) {
        hook::run();
        return Ok(());
    }
    let config = load_config(cli.config.as_deref())?;

    let resolved_agents = banto_core::config::resolve_agents(&config.agents);

    match cli.command {
        Some(Command::List) => {
            let claude_home = resolve_claude_home(cli.claude_home, &config)?;
            let codex_home = resolve_codex_home(cli.codex_home, &config);
            let thresholds = thresholds_from(&config.activity);
            run_list(
                &claude_home,
                codex_home.as_ref(),
                &thresholds,
                &resolved_agents.enabled,
            )
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
        Some(Command::Hook) => unreachable!("Command::Hook returns before load_config runs"),
        None => {
            let claude_home = resolve_claude_home(cli.claude_home, &config)?;
            let codex_home = resolve_codex_home(cli.codex_home, &config);
            let thresholds = thresholds_from(&config.activity);
            // `Store::set_session_group` (the `g` modal) takes `&mut self`
            // (it wraps a transaction), and the store is shared by both the
            // chōba TUI and emporium, so a `RefCell` gives interior
            // mutability without threading `&mut Store` through every handler.
            let store = std::cell::RefCell::new(open_store(&config)?);
            if cli.emporium {
                embedded::run_emporium(
                    &claude_home,
                    codex_home.as_ref(),
                    &thresholds,
                    &store,
                    &embedded::EmporiumSettings {
                        brigade: &config.brigade,
                        keys: &config.keys,
                        agent_binaries: &config.agent_binaries,
                        resolved_agents: &resolved_agents,
                    },
                )
            } else {
                tui::run(
                    &claude_home,
                    codex_home,
                    config.agent_binaries.clone(),
                    &thresholds,
                    config.opener,
                    &store,
                    resolved_agents,
                )
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

/// Resolve the Codex home directory per the same priority order as
/// [`resolve_claude_home`], except that failing to resolve one is not a
/// startup error: Codex support is optional, so no Codex home just means no
/// Codex sessions to show.
fn resolve_codex_home(flag: Option<PathBuf>, config: &Config) -> Option<CodexHome> {
    flag.or_else(|| config.codex_home.clone())
        .map(CodexHome::new)
        .or_else(CodexHome::default_home)
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
fn run_list(
    claude_home: &ClaudeHome,
    codex_home: Option<&CodexHome>,
    thresholds: &AgeThresholds,
    enabled_agents: &BTreeSet<AgentKind>,
) -> Result<()> {
    let rows = load_rows(claude_home, codex_home, thresholds, enabled_agents)
        .context("failed to read sessions")?;
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
