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

mod app;
mod opener;
mod process;
mod session;
mod sgr;
mod tui;
mod wrap;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use banto_core::config::{self, Config};
use banto_core::provider::claude_code::ClaudeCodeProvider;
use banto_core::status::AgeThresholds;
use banto_core::store::Store;

use process::SystemProcessRunner;
use session::{activity_tag, load_rows, thresholds_from};

/// Search and resume local Claude Code sessions.
#[derive(Parser)]
#[command(name = "banto", version, about, long_about = None)]
struct Cli {
    /// Override the Claude home directory (default: config, else ~/.claude).
    /// Read-only: banto never writes under this path.
    #[arg(long, global = true, value_name = "PATH")]
    claude_home: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print all sessions as plain text (newest first), one per line.
    List,
    /// Internal: supervises a resumed session's process (registers its PID,
    /// waits for it to exit, then cleans up). The opener spawns this; it is
    /// not meant to be invoked directly.
    #[command(name = "_wrap", hide = true)]
    Wrap {
        /// The session id being resumed.
        #[arg(long)]
        session: String,
        /// The command to run, e.g. `claude --resume <id>`.
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = load_config();

    match cli.command {
        Some(Command::List) => {
            let claude_home = resolve_claude_home(cli.claude_home, &config)?;
            let thresholds = thresholds_from(&config.activity);
            run_list(&claude_home, &thresholds)
        }
        Some(Command::Wrap { session, argv }) => {
            let store = open_store(&config)?;
            let code = wrap::run(&store, &session, &argv, &SystemProcessRunner)?;
            std::process::exit(code)
        }
        None => {
            let claude_home = resolve_claude_home(cli.claude_home, &config)?;
            let thresholds = thresholds_from(&config.activity);
            let store = open_store(&config)?;
            tui::run(&claude_home, &thresholds, config.opener, &store)
        }
    }
}

/// Load banto's own config, falling back to defaults if it is missing or the
/// platform has no config dir. A broken config never blocks startup.
fn load_config() -> Config {
    match config::default_config_path() {
        Some(path) => config::load_or_default(&path),
        None => Config::default(),
    }
}

/// Resolve the Claude home directory per the documented priority order.
fn resolve_claude_home(flag: Option<PathBuf>, config: &Config) -> Result<PathBuf> {
    flag.or_else(|| config.claude_home.clone())
        .or_else(ClaudeCodeProvider::default_home)
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
fn run_list(claude_home: &Path, thresholds: &AgeThresholds) -> Result<()> {
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
