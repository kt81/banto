//! banto binary: a clap CLI whose default action launches the TUI.
//!
//! Claude-home resolution order (highest priority first):
//! 1. the `--claude-home` flag,
//! 2. `Config.claude_home` from banto's own `config.toml`,
//! 3. the provider default (`~/.claude`).
//!
//! Everything under the resolved Claude home is read strictly read-only.

mod app;
mod session;
mod tui;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use banto_core::config::{self, Config};
use banto_core::provider::claude_code::ClaudeCodeProvider;
use banto_core::status::AgeThresholds;

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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = load_config();
    let claude_home = resolve_claude_home(cli.claude_home, &config)?;
    let thresholds = thresholds_from(&config.activity);

    match cli.command {
        Some(Command::List) => run_list(&claude_home, &thresholds),
        None => tui::run(&claude_home, &thresholds),
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
