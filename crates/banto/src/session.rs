//! Session rows for the CLI/TUI: discovery + activity classification.
//!
//! This module wires the `banto-core` provider, status and config modules
//! together into a flat [`SessionRow`] list that both the `list` subcommand
//! and the TUI render. It performs read-only work only; nothing here writes
//! to disk. Everything under `claude_home` is treated as strictly read-only.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use banto_core::config::ActivityConfig;
use banto_core::model::{Activity, AgeBucket};
use banto_core::provider::claude_code::ClaudeCodeProvider;
use banto_core::provider::{ProviderError, SessionProvider};
use banto_core::status::{self, AgeThresholds, SysinfoProbe};

/// Number of seconds in one hour.
const SECS_PER_HOUR: u64 = 60 * 60;
/// Number of seconds in one day.
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;

/// One session as shown in the list: its metadata plus computed activity.
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// Session id (UUID for Claude Code).
    pub id: String,
    /// Best-effort title (`None` when it could not be extracted).
    pub title: Option<String>,
    /// Working directory the session ran in, if known.
    pub cwd: Option<PathBuf>,
    /// Activity state driving the colored dot.
    pub activity: Activity,
}

impl SessionRow {
    /// Text used as the search haystack: `title + " " + cwd`.
    pub fn haystack(&self) -> String {
        let title = self.title.as_deref().unwrap_or("");
        format!("{title} {}", self.cwd_display())
    }

    /// Title shown in the list, falling back to the session id.
    pub fn display_title(&self) -> &str {
        match self.title.as_deref() {
            Some(title) if !title.is_empty() => title,
            _ => &self.id,
        }
    }

    /// cwd rendered as a string (empty when unknown).
    pub fn cwd_display(&self) -> String {
        self.cwd
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    }
}

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

/// Discover sessions under `claude_home`, classify their activity, and return
/// them sorted by mtime descending (newest first), with session id as a
/// deterministic tie-breaker.
///
/// Read-only: this reads `<claude_home>/projects` and `<claude_home>/sessions`
/// and never writes anywhere.
pub fn load_rows(
    claude_home: &Path,
    thresholds: &AgeThresholds,
) -> Result<Vec<SessionRow>, ProviderError> {
    let provider = ClaudeCodeProvider::new(claude_home.to_path_buf());
    let mut metas = provider.discover()?;
    metas.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.0.cmp(&b.id.0)));

    let live = status::read_live_sessions(&claude_home.join("sessions"));
    let probe = SysinfoProbe;
    let now = SystemTime::now();

    let rows = metas
        .into_iter()
        .map(|meta| {
            let activity = status::classify(&meta, &live, &probe, now, thresholds);
            SessionRow {
                id: meta.id.0,
                title: meta.title,
                cwd: meta.cwd,
                activity,
            }
        })
        .collect();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haystack_joins_title_and_cwd() {
        let row = SessionRow {
            id: "id1".into(),
            title: Some("Fix login".into()),
            cwd: Some(PathBuf::from("/work/app")),
            activity: Activity::Alive,
        };
        assert_eq!(row.haystack(), "Fix login /work/app");
    }

    #[test]
    fn haystack_tolerates_missing_fields() {
        let row = SessionRow {
            id: "id1".into(),
            title: None,
            cwd: None,
            activity: Activity::Alive,
        };
        assert_eq!(row.haystack(), " ");
    }

    #[test]
    fn display_title_falls_back_to_id() {
        let row = SessionRow {
            id: "the-id".into(),
            title: None,
            cwd: None,
            activity: Activity::Alive,
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

        let rows = load_rows(dir.path(), &AgeThresholds::default()).unwrap();
        let titles: Vec<_> = rows.iter().map(|r| r.display_title().to_string()).collect();
        assert_eq!(titles, vec!["New".to_string(), "Old".to_string()]);
    }

    /// Set a file's mtime without pulling in an extra crate: reopen and write
    /// is not enough for a deterministic value, so use `std::fs` + a manual
    /// `set_modified` via `File::set_modified` (stable since Rust 1.75).
    fn filetime_set(path: &Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }
}
