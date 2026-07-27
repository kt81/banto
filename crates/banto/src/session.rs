//! Session rows for the CLI/TUI: discovery + activity classification.
//!
//! This module wires `banto-io`'s provider/status and `banto-core`'s config
//! modules together into a flat [`SessionRow`] list that both the `list`
//! subcommand and the TUI render. It performs read-only work only; nothing
//! here writes to disk. Everything under `claude_home` is treated as
//! strictly read-only.

use std::time::{Duration, SystemTime};

use banto_core::config::ActivityConfig;
pub use banto_core::model::SessionRow;
use banto_core::model::{Activity, AgeBucket, SessionMeta};
use banto_core::status::AgeThresholds;
use banto_io::claude_home::ClaudeHome;
use banto_io::codex_home::CodexHome;
use banto_io::provider::claude_code::ClaudeCodeProvider;
use banto_io::provider::codex::CodexProvider;
use banto_io::provider::{ProviderError, SessionProvider};
use banto_io::status::{self, SysinfoProbe};

/// Number of seconds in one hour.
const SECS_PER_HOUR: u64 = 60 * 60;
/// Number of seconds in one day.
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;

/// Convert the config's [`ActivityConfig`] into status [`AgeThresholds`].
///
/// Saturating arithmetic keeps absurd config values from overflowing.
pub fn thresholds_from(activity: &ActivityConfig) -> AgeThresholds {
    AgeThresholds {
        today: Duration::from_secs(activity.today_hours.saturating_mul(SECS_PER_HOUR)),
        week: Duration::from_secs(activity.week_days.saturating_mul(SECS_PER_DAY)),
    }
}

/// Short plain-text tag for the `list` subcommand.
pub fn activity_tag(activity: Activity) -> &'static str {
    match activity {
        Activity::Busy => "busy",
        Activity::Alive => "alive",
        Activity::Idle(AgeBucket::Today) => "today",
        Activity::Idle(AgeBucket::ThisWeek) => "week",
        Activity::Idle(AgeBucket::Older) => "older",
    }
}

/// Discover sessions under `claude_home` (and `codex_home`, when resolved),
/// classify their activity, and return them sorted by mtime descending
/// (newest first), with session id as a deterministic tie-breaker. A thin
/// wrapper — [`discover_all`], then [`rows_from_metas`] — kept for callers
/// that only want rows and have no reason to run a second discovery pass of
/// their own.
///
/// Read-only: this reads `<claude_home>/projects`, `<claude_home>/sessions`,
/// and `codex_home`'s `threads` database, and never writes anywhere.
pub fn load_rows(
    claude_home: &ClaudeHome,
    codex_home: Option<&CodexHome>,
    thresholds: &AgeThresholds,
) -> Result<Vec<SessionRow>, ProviderError> {
    let metas = discover_all(claude_home, codex_home)?;
    Ok(rows_from_metas(metas, claude_home, thresholds))
}

/// Every session both providers know about, merged (not deduplicated: the
/// two products never share a session id). Codex is skipped entirely when
/// `codex_home` is `None` — an absent Codex home degrades to "no Codex
/// sessions", not an error, the same way a missing `threads` database
/// inside [`CodexProvider::discover`] does.
pub fn discover_all(
    claude_home: &ClaudeHome,
    codex_home: Option<&CodexHome>,
) -> Result<Vec<SessionMeta>, ProviderError> {
    let mut metas = ClaudeCodeProvider::new(claude_home.clone()).discover()?;
    if let Some(codex_home) = codex_home {
        metas.extend(CodexProvider::new(codex_home.clone()).discover()?);
    }
    Ok(metas)
}

/// The conversion half of [`load_rows`] — classify and sort already-
/// discovered `metas` into [`SessionRow`]s — extracted so a caller that has
/// its own discover() pass to feed elsewhere too (e.g. lineage resolution —
/// see `crate::tui::superseded_from_metas`) doesn't need a second discover()
/// just for this. `claude_home`/`thresholds` are still needed here: they
/// drive the live-status read and activity classification, neither of which
/// discovery itself touches.
pub fn rows_from_metas(
    mut metas: Vec<SessionMeta>,
    claude_home: &ClaudeHome,
    thresholds: &AgeThresholds,
) -> Vec<SessionRow> {
    metas.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.0.cmp(&b.id.0)));

    let live = status::read_live_sessions(&claude_home.sessions_dir());
    let probe = SysinfoProbe;
    let now = SystemTime::now();

    metas
        .into_iter()
        .map(|meta| {
            let activity = status::classify(&meta, &live, &probe, now, thresholds);
            SessionRow {
                id: meta.id.0,
                agent: meta.agent,
                title: meta.title,
                cwd: meta.cwd,
                activity,
                is_agent: meta.is_agent,
                preview: meta.preview,
                mtime: meta.mtime,
                size: meta.size,
                source_archived: meta.source_archived,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use banto_core::model::{AgentKind, SessionId};

    use super::*;

    /// A synthetic [`SessionMeta`], overriding just id/mtime — the rest are
    /// values [`rows_from_metas`]'s tests can assert carry straight through.
    fn meta(id: &str, mtime: SystemTime) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.to_string()),
            agent: AgentKind::ClaudeCode,
            title: Some(format!("Title {id}")),
            cwd: None,
            source_path: PathBuf::from(format!("{id}.jsonl")),
            mtime,
            size: 42,
            is_agent: false,
            preview: None,
            continuation_of_uuid: None,
            source_archived: false,
        }
    }

    #[test]
    fn haystack_joins_title_and_cwd() {
        let row = SessionRow {
            id: "id1".into(),
            agent: AgentKind::ClaudeCode,
            title: Some("Fix login".into()),
            cwd: Some(PathBuf::from("/work/app")),
            activity: Activity::Alive,
            is_agent: false,
            preview: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
        };
        assert_eq!(row.haystack(), "Fix login /work/app");
    }

    #[test]
    fn haystack_tolerates_missing_fields() {
        let row = SessionRow {
            id: "id1".into(),
            agent: AgentKind::ClaudeCode,
            title: None,
            cwd: None,
            activity: Activity::Alive,
            is_agent: false,
            preview: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
        };
        assert_eq!(row.haystack(), " ");
    }

    #[test]
    fn display_title_falls_back_to_id() {
        let row = SessionRow {
            id: "the-id".into(),
            agent: AgentKind::ClaudeCode,
            title: None,
            cwd: None,
            activity: Activity::Alive,
            is_agent: false,
            preview: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
        };
        assert_eq!(row.display_title(), "the-id");
    }

    #[test]
    fn thresholds_convert_from_config() {
        let config = ActivityConfig {
            today_hours: 12,
            week_days: 3,
        };
        let thresholds = thresholds_from(&config);
        assert_eq!(thresholds.today, Duration::from_secs(12 * SECS_PER_HOUR));
        assert_eq!(thresholds.week, Duration::from_secs(3 * SECS_PER_DAY));
    }

    #[test]
    fn default_config_maps_to_default_thresholds() {
        let thresholds = thresholds_from(&ActivityConfig::default());
        assert_eq!(thresholds, AgeThresholds::default());
    }

    #[test]
    fn activity_tags_cover_every_variant() {
        assert_eq!(activity_tag(Activity::Busy), "busy");
        assert_eq!(activity_tag(Activity::Alive), "alive");
        assert_eq!(activity_tag(Activity::Idle(AgeBucket::Today)), "today");
        assert_eq!(activity_tag(Activity::Idle(AgeBucket::ThisWeek)), "week");
        assert_eq!(activity_tag(Activity::Idle(AgeBucket::Older)), "older");
    }

    #[test]
    fn load_rows_sorts_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects").join("proj");
        std::fs::create_dir_all(&projects).unwrap();
        // Two synthetic sessions; write "old" first, then "new" later so its
        // mtime is strictly greater regardless of filesystem granularity.
        std::fs::write(
            projects.join("old.jsonl"),
            "{\"type\":\"custom-title\",\"customTitle\":\"Old\"}\n",
        )
        .unwrap();
        // Nudge mtimes apart deterministically.
        let older = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
        filetime_set(&projects.join("old.jsonl"), older);
        std::fs::write(
            projects.join("new.jsonl"),
            "{\"type\":\"custom-title\",\"customTitle\":\"New\"}\n",
        )
        .unwrap();
        filetime_set(&projects.join("new.jsonl"), newer);

        let claude_home = ClaudeHome::new(dir.path().to_path_buf());
        let rows = load_rows(&claude_home, None, &AgeThresholds::default()).unwrap();
        let titles: Vec<_> = rows.iter().map(|r| r.display_title().to_string()).collect();
        assert_eq!(titles, vec!["New".to_string(), "Old".to_string()]);
    }

    #[test]
    fn discover_all_merges_claude_and_codex_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let claude_root = dir.path().join("claude");
        let projects = claude_root.join("projects").join("proj");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(
            projects.join("claude-1.jsonl"),
            "{\"type\":\"custom-title\",\"customTitle\":\"Claude session\"}\n",
        )
        .unwrap();

        let codex_root = dir.path().join("codex");
        std::fs::create_dir_all(&codex_root).unwrap();
        let rollout = codex_root.join("rollout.jsonl");
        std::fs::write(&rollout, "x").unwrap();
        let codex_home = banto_io::codex_home::CodexHome::new(codex_root);
        let conn = rusqlite::Connection::open(codex_home.threads_db_path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT, cwd TEXT, \
             rollout_path TEXT, first_user_message TEXT, updated_at_ms INTEGER, \
             archived INTEGER DEFAULT 0);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, title, rollout_path, updated_at_ms) \
             VALUES ('codex-1', 'Codex session', ?1, 0)",
            rusqlite::params![rollout.to_string_lossy()],
        )
        .unwrap();
        drop(conn); // clean close: no -wal left behind for discover() to open around

        let claude_home = ClaudeHome::new(claude_root);
        let metas = discover_all(&claude_home, Some(&codex_home)).unwrap();
        let mut ids: Vec<_> = metas.iter().map(|m| m.id.0.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["claude-1".to_string(), "codex-1".to_string()]);
        let agents: Vec<_> = metas.iter().map(|m| m.agent).collect();
        assert!(agents.contains(&AgentKind::ClaudeCode));
        assert!(agents.contains(&AgentKind::Codex));
    }

    #[test]
    fn discover_all_skips_codex_entirely_when_its_home_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let claude_home = ClaudeHome::new(dir.path().to_path_buf());
        let metas = discover_all(&claude_home, None).unwrap();
        assert!(metas.is_empty());
    }

    #[test]
    fn rows_from_metas_sorts_newest_first_and_carries_fields_through() {
        // No `.jsonl` files on disk at all here — unlike
        // `load_rows_sorts_newest_first`, this is the extracted conversion
        // half in isolation: already-discovered `metas` in, `SessionRow`s
        // out, no discover() of its own (that's the point of the split).
        let dir = tempfile::tempdir().unwrap();
        let older = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
        let metas = vec![meta("old", older), meta("new", newer)];

        let claude_home = ClaudeHome::new(dir.path().to_path_buf());
        let rows = rows_from_metas(metas, &claude_home, &AgeThresholds::default());

        let ids: Vec<_> = rows.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids, vec!["new".to_string(), "old".to_string()]);
        assert_eq!(rows[0].title.as_deref(), Some("Title new"));
        assert_eq!(rows[0].agent, AgentKind::ClaudeCode);
        assert_eq!(rows[0].size, 42);
    }

    /// Set a file's mtime without pulling in an extra crate: reopen and write
    /// is not enough for a deterministic value, so use `std::fs` + a manual
    /// `set_modified` via `File::set_modified` (stable since Rust 1.75).
    fn filetime_set(path: &Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }
}
