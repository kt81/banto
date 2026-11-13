//! banto binary: a clap CLI whose default action launches the TUI — the session
//! list and the hosted panes together (`embedded::run_emporium`).
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
//! All of this, and where `config.toml` and banto's own database are, is decided
//! once in [`startup`]. The two homes differ only in failure mode: no Claude home
//! is a startup error (`--claude-home` is the escape hatch), while no Codex home
//! just means no Codex sessions — Codex support is optional, Claude's is not.
//!
//! Everything under the resolved Claude home, and under the resolved Codex
//! home, is treated as strictly read-only.
//! banto's own database (groups, pins, archive, brigades) lives under
//! `Config.db_path`, falling back to `banto.db` in
//! `banto_io::data_dir::DataDir::default_root`.
//!
//! `config.toml` itself is located by `banto_io::config::resolve_config_path`'s
//! resolution order. A banto that was itself started by another banto (`_mcp`,
//! `_hook`) is told what its parent resolved and uses that instead of
//! resolving for itself: see [`startup`].
//!
//! `Config.agents` gates which products get discovered at all — resolved
//! once here via `banto_core::config::resolve_agents` into `resolved_agents`
//! and passed down to every entry point, since `session::discover_all` (not
//! any of these) is what actually decides which provider runs.
//! `ResolvedAgents::ignored` is where a name banto didn't recognize ends up;
//! `embedded::run_emporium` is what tells the operator about it, once, at
//! startup — see `session::agents_ignored_notice`.

mod briefing;
mod codex_trust;
mod embedded;
mod hook;
mod launch;
mod mcp;
mod session;
mod startup;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use banto_core::model::{AgentKind, BrigadeRole};
use banto_core::status::AgeThresholds;
use banto_io::agent_versions::{
    read_claude_last_auto_update, read_codex_current_version, read_codex_release_history,
    read_codex_session_version,
};
use banto_io::claude_home::ClaudeHome;
use banto_io::codex_home::CodexHome;
use banto_io::codex_liveness::{SysinfoStartTime, live_thread_ids};
use banto_io::provider::SessionProvider;
use banto_io::provider::codex::CodexProvider;
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
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        conflicts_with_all = ["parent_config", "parent_no_config"]
    )]
    config: Option<PathBuf>,

    /// Internal: the database path a parent banto resolved. Passed to every
    /// banto process the parent starts (`_mcp`), so none of them
    /// re-resolves it from its own environment and working directory.
    #[arg(long, global = true, hide = true, value_name = "PATH")]
    db_path: Option<PathBuf>,

    /// Internal: banto's data directory as a parent banto resolved it — where
    /// MCP configs, consultation requests and message logs live. Passed like
    /// `--db-path`.
    #[arg(long, global = true, hide = true, value_name = "PATH")]
    data_dir: Option<PathBuf>,

    /// Internal: the config file a parent banto loaded, loaded again the same
    /// way. Passed like `--db-path`.
    #[arg(
        long,
        global = true,
        hide = true,
        value_name = "PATH",
        conflicts_with = "parent_no_config"
    )]
    parent_config: Option<PathBuf>,

    /// Internal: the parent banto loaded no config file (defaults).
    #[arg(long, global = true, hide = true)]
    parent_no_config: bool,

    /// Accepted and ignored: banto has one UI, which is what this used to select.
    /// Kept only so existing launch habits (shortcuts, scripts) keep working;
    /// to be removed.
    #[arg(long = "emporium", alias = "oodana", hide = true)]
    _emporium: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print all sessions as plain text (newest first), one per line.
    List,
    /// Pull and print agent version facts retained by Claude Code and Codex.
    /// This never changes banto's session list, tiles, or sidebar.
    Versions,
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
        /// (`director`, `worker-1`, `worker-2`, ..., or `goinkyo`).
        #[arg(long)]
        member: Option<String>,
        /// This session's role in the brigade: `director`, `worker` or `goinkyo`.
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

impl Cli {
    /// `--config` next to a parent's hand-off is a contradiction (banto never
    /// generates it). clap rejects it when both flags sit on the same side of the
    /// subcommand name; a global flag given before the subcommand and another
    /// after it are parsed separately, so this covers that placement too.
    fn check_hand_off_flags(&self) -> std::result::Result<(), &'static str> {
        if self.config.is_some() && (self.parent_config.is_some() || self.parent_no_config) {
            return Err("--config cannot be used with --parent-config or --parent-no-config");
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Err(message) = cli.check_hand_off_flags() {
        use clap::CommandFactory;
        Cli::command()
            .error(clap::error::ErrorKind::ArgumentConflict, message)
            .exit();
    }
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
    let inputs = startup::Inputs {
        cli_config: cli.config.clone(),
        cli_claude_home: cli.claude_home.clone(),
        cli_codex_home: cli.codex_home.clone(),
        cli_db_path: cli.db_path.clone(),
        cli_data_dir: cli.data_dir.clone(),
        parent_config: cli.parent_config.clone(),
        parent_no_config: cli.parent_no_config,
        env: startup::Env::from_process(),
        // Optional: a working directory that cannot be read only leaves relative
        // paths as they were given, as before.
        cwd: std::env::current_dir().ok(),
    };
    let (config, resolved) = startup::resolve(&inputs, startup::Strictness::Strict)?;

    let resolved_agents = banto_core::config::resolve_agents(&config.agents);

    match cli.command {
        Some(Command::List) => {
            let claude_home = resolved.claude_home()?;
            let codex_home = resolved.codex_home();
            let thresholds = thresholds_from(&config.activity);
            run_list(
                &claude_home,
                codex_home.as_ref(),
                &thresholds,
                &resolved_agents.enabled,
            )
        }
        Some(Command::Versions) => {
            let claude_home = resolved.claude_home()?;
            let codex_home = resolved.codex_home();
            run_versions(&claude_home, codex_home.as_ref())
        }
        Some(Command::Mcp {
            session,
            brigade,
            member,
            role,
        }) => {
            let store = open_store(&resolved)?;
            let claude_home = resolved.claude_home()?;
            let codex_home = resolved.codex_home();
            // The data directory is the parent's (`--data-dir`), so the consultation
            // request written here, the member configs and the message log all sit
            // where the emporium that reads and sweeps them looks.
            let data_dir = resolved.data_dir();
            let goinkyo_dir = data_dir.as_ref().map(|dir| dir.goinkyo_dir());
            let identity = mcp::Identity {
                session,
                brigade,
                member,
                role: role.as_deref().and_then(BrigadeRole::from_token),
            };
            // Opt-in; the directory is banto's own, whatever the config says.
            let message_log = mcp::message_log_for(
                config.brigade.message_log,
                data_dir.as_ref().map(|dir| dir.logs_dir()),
            );
            mcp::run_stdio_server(
                store,
                identity,
                claude_home,
                codex_home,
                goinkyo_dir,
                message_log,
            )
        }
        Some(Command::Hook) => unreachable!("Command::Hook returns before load_config runs"),
        None => {
            let claude_home = resolved.claude_home()?;
            let codex_home = resolved.codex_home();
            let thresholds = thresholds_from(&config.activity);
            // `Store`'s write methods take `&mut self` (they wrap a transaction)
            // and the store is shared by the event loop and its executors, so a
            // `RefCell` gives interior mutability without threading `&mut Store`
            // through every handler.
            let store = std::cell::RefCell::new(open_store(&resolved)?);
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
                    child: &resolved,
                },
            )
        }
    }
}

/// Open (creating if needed) banto's own sqlite database at the resolved path.
fn open_store(resolved: &startup::Resolved) -> Result<Store> {
    Store::open(resolved.db_path()?).context("failed to open banto's database")
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

/// `banto versions`: a deliberately pull-only juxtaposition of upstream
/// version facts.  It does not persist a second ledger: Codex retains its
/// release history itself, while Claude retains only its most recent
/// automatic-update result.
fn run_versions(claude_home: &ClaudeHome, codex_home: Option<&CodexHome>) -> Result<()> {
    println!("Claude Code");
    match read_claude_last_auto_update(&claude_home.last_update_result_path()) {
        Some(update) => {
            let from = update.version_from.as_deref().unwrap_or("unknown");
            let to = update.version_to.as_deref().unwrap_or("unknown");
            println!(
                "  last auto-update attempt by a Claude Code binary: {from} -> {to} at {}",
                update.timestamp
            );
        }
        None => println!("  last auto-update attempt by a Claude Code binary: unknown"),
    }
    println!(
        "  history before that last hop is not retained upstream; banto does not fabricate it."
    );
    println!(
        "  currently running session versions (not listed means no live process was detected):"
    );
    let live = banto_io::status::read_live_sessions(&claude_home.sessions_dir());
    if live.is_empty() {
        println!("    none detected");
    }
    for session in live {
        let id = session
            .session_id
            .unwrap_or_else(|| format!("pid {}", session.pid));
        println!(
            "    {id}: {}",
            session.version.as_deref().unwrap_or("unknown")
        );
    }

    println!("Codex");
    let Some(codex_home) = codex_home else {
        println!("  current standalone package: unknown");
        println!("  release history: unavailable (no Codex home)");
        println!("  session versions: unknown");
        return Ok(());
    };
    match read_codex_current_version(&codex_home.standalone_current_package_path()) {
        Some(version) => println!("  current standalone package: {version}"),
        None => println!("  current standalone package: unknown"),
    }
    println!("  retained standalone release history (landed on this machine):");
    let releases = read_codex_release_history(&codex_home.standalone_releases_dir());
    if releases.is_empty() {
        println!("    unknown");
    } else {
        for release in releases {
            println!(
                "    {} at {}",
                release.version,
                format_timestamp(release.modified)
            );
        }
    }
    println!(
        "  currently running session CLI versions (not listed means no live process was detected):"
    );
    // This is intentionally not `gather_reload`/discovery work.  Codex
    // discovery reads its sqlite index and only metadata of each rollout;
    // this pull reads a rollout head solely to expose session_meta.cli_version.
    let metas = CodexProvider::new(codex_home.clone())
        .discover()
        .unwrap_or_default();
    // Resolve liveness in one logs-db connection, then pay rollout-head I/O
    // only for the small running subset rather than every retained session.
    let live_codex = live_thread_ids(codex_home, &SysinfoStartTime);
    let has_codex_sessions = metas
        .iter()
        .any(|meta| meta.agent == AgentKind::Codex && live_codex.contains(&meta.id.0));
    for meta in metas
        .into_iter()
        .filter(|meta| meta.agent == AgentKind::Codex && live_codex.contains(&meta.id.0))
    {
        let version =
            read_codex_session_version(&meta.source_path).unwrap_or_else(|| "unknown".into());
        println!("    {}: {version}", meta.id);
    }
    if !has_codex_sessions {
        println!("    none detected");
    }
    Ok(())
}

fn format_timestamp(time: Option<SystemTime>) -> String {
    let Some(duration) = time.and_then(|time| time.duration_since(UNIX_EPOCH).ok()) else {
        return "unknown".to_string();
    };
    let seconds = duration.as_secs();
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_date_from_unix_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60
    )
}

/// Gregorian civil date for a count of whole days since 1970-01-01.
/// Kept local instead of adding a time-formatting dependency for one
/// pull-only display line.
fn civil_date_from_unix_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_prime = (5 * doy + 2) / 153;
    let day = doy - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn formats_release_time_as_an_iso_utc_timestamp() {
        assert_eq!(
            format_timestamp(Some(UNIX_EPOCH + Duration::from_secs(1_786_066_319))),
            "2026-08-07T01:31:59Z"
        );
    }

    #[test]
    fn pre_epoch_release_time_is_unknown() {
        assert_eq!(
            format_timestamp(Some(UNIX_EPOCH - Duration::from_secs(1))),
            "unknown"
        );
    }
}
