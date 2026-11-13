//! Session rows for the CLI/TUI: discovery + activity classification.
//!
//! This module wires `banto-io`'s provider/status and `banto-core`'s config
//! modules together into a flat [`SessionRow`] list that both the `list`
//! subcommand and the TUI render. It performs read-only work only; nothing
//! here writes to disk. Everything under `claude_home` is treated as
//! strictly read-only.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, SystemTime};

use banto_core::config::{ActivityConfig, ResolvedAgents};
pub use banto_core::model::SessionRow;
use banto_core::model::{Activity, AgeBucket, AgentKind, SessionId, SessionMeta};
use banto_core::status::{AgeThresholds, age_bucket};
use banto_io::claude_home::ClaudeHome;
use banto_io::codex_home::CodexHome;
use banto_io::codex_liveness::live_candidate_thread_ids;
use banto_io::codex_trust::HookTrustState;
use banto_io::lineage::resolve_lineage;
use banto_io::provider::cache::MetaCache;
use banto_io::provider::claude_code::ClaudeCodeProvider;
use banto_io::provider::codex::CodexProvider;
use banto_io::provider::{ProviderError, SessionProvider};
use banto_io::status::{self, SysinfoProbe};
use banto_io::store::Store;

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
        Activity::Waiting => "waiting",
        Activity::Alive => "alive",
        Activity::Idle(AgeBucket::Today) => "today",
        Activity::Idle(AgeBucket::ThisWeek) => "week",
        Activity::Idle(AgeBucket::Older) => "older",
    }
}

/// The startup notice for `Config.agents`, when [`resolve_agents`] had to
/// ignore part of it — `None` when every name parsed cleanly (including the
/// ordinary `all`/unset case, which never has anything to ignore). Shared
/// by `crate::embedded::run_emporium`, the entry point with a status line to
/// put this in; the `list` subcommand has no such line and doesn't call this.
///
/// Fires for a partial drop too, not only the total fallback: even when
/// some names were recognized and the setting still did something real, a
/// silently-dropped name is still a typo the operator would want to know
/// about — `ResolvedAgents::fell_back_to_all` only changes the wording
/// (naming what's still in effect vs. saying the setting did nothing at
/// all), not whether the notice appears.
///
/// [`resolve_agents`]: banto_core::config::resolve_agents
pub fn agents_ignored_notice(resolved: &ResolvedAgents) -> Option<String> {
    if resolved.ignored.is_empty() {
        return None;
    }
    let names = resolved.ignored.join(", ");
    Some(if resolved.fell_back_to_all {
        format!("agents: no recognized name in \"{names}\" — showing every product")
    } else {
        let kept = resolved
            .enabled
            .iter()
            .map(|kind| kind.label())
            .collect::<Vec<_>>()
            .join(", ");
        format!("agents: ignored unknown name(s) \"{names}\" — showing {kept}")
    })
}

/// Status-line notice telling the operator that Codex has not been asked to
/// trust banto's brigade hook yet, or `None` when the question doesn't apply.
///
/// Deliberately raised *before* a brigade is staged rather than after it goes
/// wrong. An untrusted hook is not refused loudly — Codex drops it in silence
/// (docs/notes/codex-briefing-spike.md), so the first sign of trouble would
/// otherwise be members behaving as though nobody had told them they were in
/// a cell. And the approval prompt cannot be answered once for a whole
/// brigade: trust is read as each member starts, so members launched together
/// each raise their own dialog, and dismissing any of them leaves that member
/// briefed by nothing.
///
/// [`HookTrustState::Unknown`] is treated exactly like `NotPrimed` — banto
/// could not tell, and offering a one-time step the operator may not need
/// costs less than staying silent about one they do. `Primed` stays quiet
/// while being only a hint; see [`banto_io::codex_trust`] for why it cannot
/// be more than that, and `BrigadeMember::briefed_at` for the fact that can.
pub fn codex_trust_notice(
    state: HookTrustState,
    codex_enabled: bool,
    has_brigade: bool,
    hook_launchable: bool,
) -> Option<String> {
    if !codex_enabled || !has_brigade {
        return None;
    }
    // Outranks the trust question, and outranks `Primed` too: from a path
    // Codex cannot launch, an existing trust record is a leftover for a hook
    // that will never run, so offering a review pane (which the emporium's
    // own `Modal::ConfirmCodexTrust` skips for the same reason — see
    // `update_codex_trust_checked`) would only ever decline to open.
    if !hook_launchable {
        return Some(
            "codex: banto's own path contains a space, which Codex cannot launch a hook \
             from — brigade members stay unbriefed until banto is moved somewhere without one"
                .to_string(),
        );
    }
    if state == HookTrustState::Primed {
        return None;
    }
    Some(
        "codex: brigade briefings need a one-time approval — forming a brigade with a \
         Codex member will offer to open a review pane first (until then a Codex member \
         starts unbriefed, without saying so)"
            .to_string(),
    )
}

/// Discover sessions under `claude_home` (and `codex_home`, when resolved
/// and enabled), classify their activity, and return them sorted by mtime
/// descending (newest first), with session id as a deterministic
/// tie-breaker. A thin wrapper — [`discover_all`], then [`rows_from_metas`]
/// — kept for callers that only want rows and have no reason to run a
/// second discovery pass of their own.
///
/// Read-only: this reads `<claude_home>/projects`, `<claude_home>/sessions`,
/// and `codex_home`'s `threads` database, and never writes anywhere.
pub fn load_rows(
    claude_home: &ClaudeHome,
    codex_home: Option<&CodexHome>,
    thresholds: &AgeThresholds,
    enabled: &BTreeSet<AgentKind>,
) -> Result<Vec<SessionRow>, ProviderError> {
    // No cache: every caller of this wrapper walks once and exits (`banto
    // list`), and a memo nothing consults twice is only a copy of the answer.
    let metas = discover_all(claude_home, codex_home, enabled, None)?;
    Ok(rows_from_metas(metas, claude_home, codex_home, thresholds))
}

/// Every session every *enabled* provider knows about, merged (not
/// deduplicated: the two products never share a session id).
///
/// `enabled` (resolved from `Config::agents` by
/// `banto_core::config::resolve_agents`) gates which provider *runs at
/// all*, not which of its already-discovered rows get kept — unlike
/// `App::reveal`/[`mark_archived`], which filter or mark
/// rows banto has already read off disk. That distinction matters here
/// specifically: `CodexProvider::discover` opens a foreign sqlite database,
/// under a read-only exception this crate had to earn (see
/// `crate::codex_home`/`crate::sqlite_ro`'s docs). An operator who has
/// switched Codex off should have banto never touch that file, not have
/// banto read it and then discard the result — so a disabled product's
/// provider is never constructed, let alone called.
///
/// Codex is additionally skipped whenever `codex_home` is `None` — an
/// absent Codex home degrades to "no Codex sessions", not an error, the
/// same way a missing `threads` database inside [`CodexProvider::discover`]
/// does — independent of whether Codex is enabled.
pub fn discover_all(
    claude_home: &ClaudeHome,
    codex_home: Option<&CodexHome>,
    enabled: &BTreeSet<AgentKind>,
    cache: Option<&MetaCache>,
) -> Result<Vec<SessionMeta>, ProviderError> {
    let mut metas = Vec::new();
    if enabled.contains(&AgentKind::ClaudeCode) {
        let provider = ClaudeCodeProvider::new(claude_home.clone());
        metas.extend(match cache {
            Some(cache) => provider.discover_cached(cache)?,
            None => provider.discover()?,
        });
    }
    if enabled.contains(&AgentKind::Codex)
        && let Some(codex_home) = codex_home
    {
        metas.extend(CodexProvider::new(codex_home.clone()).discover()?);
    }
    Ok(metas)
}

/// The Claude display/activity seam for policy that cannot enter
/// [`SysinfoProbe::alive_pids`]'s shared open/resume path.
pub(crate) fn alive_claude_pids(pids: impl IntoIterator<Item = u32>) -> HashSet<u32> {
    SysinfoProbe::alive_pids(pids)
}

/// The conversion half of [`load_rows`] — classify and sort already-
/// discovered `metas` into [`SessionRow`]s — extracted so a caller that has
/// its own discover() pass to feed elsewhere too (e.g. lineage resolution —
/// see [`superseded_from_metas`]) doesn't need a second discover()
/// just for this. `claude_home`/`thresholds` are still needed here: they
/// drive the live-status read and activity classification, neither of which
/// discovery itself touches.
pub fn rows_from_metas(
    mut metas: Vec<SessionMeta>,
    claude_home: &ClaudeHome,
    codex_home: Option<&CodexHome>,
    thresholds: &AgeThresholds,
) -> Vec<SessionRow> {
    metas.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.id.0.cmp(&b.id.0)));

    let live = status::read_live_sessions(&claude_home.sessions_dir());
    // Codex list activity deliberately stops at alive-or-aged.  Busy exists
    // in principle (`codex_activity::is_thread_idle` can read a rollout's
    // task markers) but would add a state-db open plus a tail read growing
    // toward 4 MiB per row on every debounced reload, so it is a separate
    // costed slice rather than hidden work here.  Waiting is unavailable:
    // those markers cannot distinguish a human approval pause from model
    // generation.  Do not synthesize a Claude `LiveSession` for Codex; its
    // status vocabulary would imply that unavailable Waiting state.
    let live_codex = codex_home.map_or_else(BTreeSet::new, |home| {
        live_candidate_thread_ids(
            home,
            metas
                .iter()
                .filter(|meta| meta.agent == AgentKind::Codex)
                .map(|meta| meta.id.0.clone()),
        )
    });
    let live_claude = alive_claude_pids(live.iter().map(|entry| entry.pid));
    let now = SystemTime::now();

    metas
        .into_iter()
        .map(|meta| {
            let activity = activity_from_observations(
                &meta,
                &live,
                &live_claude,
                &live_codex,
                now,
                thresholds,
            );
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
                // Discovery reads `~/.claude` and `~/.codex`; banto's own
                // archive lives in banto's store, which this never opens.
                // [`mark_archived`] fills it in from there.
                archived: false,
            }
        })
        .collect()
}

fn activity_from_observations(
    meta: &SessionMeta,
    live_claude: &[banto_io::status::LiveSession],
    alive_claude: &HashSet<u32>,
    live_codex: &BTreeSet<String>,
    now: SystemTime,
    thresholds: &AgeThresholds,
) -> Activity {
    match meta.agent {
        AgentKind::ClaudeCode => {
            status::classify_from_alive_pids(meta, live_claude, alive_claude, now, thresholds)
        }
        AgentKind::Codex if live_codex.contains(&meta.id.0) => Activity::Alive,
        AgentKind::Codex => Activity::Idle(age_bucket(meta.mtime, now, thresholds)),
    }
}

/// Drop archived sessions from `rows` — the union of two independent facts:
/// banto's own archive (soft-hide via `d` — see
/// `App::open_confirm_archive_modal`/`confirm_modal`), and, for Codex,
/// `SessionRow::source_archived` (`threads.archived`, set by `codex
/// archive`). Two facts, not one: banto's archive is per-session-id and
/// product-neutral, entirely independent of whatever the session's own
/// product thinks; `source_archived` is the reverse — a fact banto can only
/// ever read, since `~/.codex` stays read-only to it (never a write, so
/// never an unarchive from banto's side either). Either one hides the row;
/// neither is cleared by the other disagreeing. Concretely: unarchiving in
/// Codex (`codex unarchive`) clears `source_archived` on the next reload,
/// and the row reappears *unless* the operator separately archived that
/// same session in banto too, in which case it stays hidden until banto's
/// own archive is lifted — a real store write, unaffected by anything
/// Codex reports. A store read failure is tolerated: nothing gets excluded
/// rather than blocking the TUI.
pub(crate) fn mark_archived(rows: Vec<SessionRow>, store: &Store) -> Vec<SessionRow> {
    let archived: HashSet<String> = store
        .archived_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect();
    rows.into_iter()
        // `source_archived` is still dropped outright, and always will be:
        // it is the source product's own flag, banto can only read it, and a
        // row offering `D` that banto could never carry out would be worse
        // than one that is simply absent. banto's own archive is a different
        // fact — carried on the row and hidden by `Reveal`, so `D` can reach
        // it at the level that shows it.
        .filter(|row| !row.source_archived)
        .map(|mut row| {
            row.archived = archived.contains(&row.id);
            row
        })
        .collect()
}

/// Load every known group, alphabetical by name. Tolerant: a read failure
/// just means no groups are known yet, rather than blocking the TUI.
pub(crate) fn load_groups(store: &Store) -> Vec<(i64, String)> {
    let mut groups: Vec<(i64, String)> = store
        .list_groups()
        .unwrap_or_default()
        .into_iter()
        .map(|g| (g.id, g.name))
        .collect();
    groups.sort_by(|a, b| a.1.cmp(&b.1));
    groups
}

/// Load the session -> group id map by walking each group's members (fewer
/// queries than asking per-session). Tolerant: a read failure for one group
/// just means its members show as ungrouped, rather than blocking the TUI.
pub(crate) fn load_session_groups(store: &Store, groups: &[(i64, String)]) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    for &(group_id, _) in groups {
        for session_id in store.group_members(group_id).unwrap_or_default() {
            map.insert(session_id.0, group_id);
        }
    }
    map
}

/// Load the currently pinned session ids from the store. Tolerant: a read
/// failure just means no sessions start out pinned, rather than blocking the
/// TUI from starting.
pub(crate) fn load_pinned(store: &Store) -> HashSet<String> {
    store
        .pinned_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect()
}

/// Load every brigade Worker's or Goinkyo's Claude session id, across every
/// brigade, that has been assigned one so far — [`App`] hides these from the
/// list (see `App::hidden`). Tolerant: a read failure just means nothing is
/// hidden yet, rather than blocking the TUI from starting.
pub(crate) fn load_hidden_member_ids(store: &Store) -> HashSet<String> {
    store
        .brigade_hidden_session_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect()
}

/// Load every brigade Director's Claude session id, across every brigade —
/// [`App`] marks these in the list/summary (see `App::directors`). Tolerant:
/// a read failure just means no marker shows yet, rather than blocking the
/// TUI from starting.
pub(crate) fn load_directors(store: &Store) -> HashSet<String> {
    store
        .brigade_director_session_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect()
}

/// Spend this reload's lineage-resolution budget against already-discovered
/// `metas`, then return every session id with a known auto-compaction
/// continuation — [`App`] hides these (see `App::superseded`). Takes
/// `metas` rather than discovering its own: `SessionMeta`'s
/// `continuation_of_uuid` doesn't survive into [`session::load_rows`]'s
/// `SessionRow`s, but `banto_io::lineage::resolve_lineage` already wants
/// exactly `&[SessionMeta]`, so every call site — which already needs a
/// discover() pass for the rows anyway — can hand the same one here instead
/// of paying for a second. Tolerant: a lineage-resolution failure just means
/// no progress this pass, not a blocked TUI.
pub(crate) fn superseded_from_metas(
    metas: &[SessionMeta],
    store: &Store,
    failed: &RefCell<HashSet<SessionId>>,
) -> HashSet<String> {
    let mut failed = failed.borrow_mut();
    let _ = resolve_lineage(store, metas, &mut failed);
    store
        .lineage_parent_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
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
            archived: false,
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
            archived: false,
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
            archived: false,
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
        assert_eq!(activity_tag(Activity::Waiting), "waiting");
        assert_eq!(activity_tag(Activity::Alive), "alive");
        assert_eq!(activity_tag(Activity::Idle(AgeBucket::Today)), "today");
        assert_eq!(activity_tag(Activity::Idle(AgeBucket::ThisWeek)), "week");
        assert_eq!(activity_tag(Activity::Idle(AgeBucket::Older)), "older");
    }

    // -- codex_trust_notice ----------------------------------------------

    #[test]
    fn codex_trust_notice_offers_the_one_time_step_when_nothing_looks_trusted() {
        let notice = codex_trust_notice(HookTrustState::NotPrimed, true, true, true)
            .expect("an unprimed cell must be warned");
        assert!(notice.contains("review pane"), "must name the fix");
    }

    #[test]
    fn codex_trust_notice_treats_an_unreadable_config_as_unprimed() {
        // Silence here would trade a needless prompt for a cell that forms
        // with members nothing ever briefs.
        assert!(codex_trust_notice(HookTrustState::Unknown, true, true, true).is_some());
    }

    #[test]
    fn codex_trust_notice_is_silent_once_something_looks_trusted() {
        assert_eq!(
            codex_trust_notice(HookTrustState::Primed, true, true, true),
            None
        );
    }

    #[test]
    fn codex_trust_notice_is_silent_for_an_operator_with_no_brigade_or_no_codex() {
        assert_eq!(
            codex_trust_notice(HookTrustState::NotPrimed, true, false, true),
            None
        );
        assert_eq!(
            codex_trust_notice(HookTrustState::NotPrimed, false, true, true),
            None
        );
    }

    #[test]
    fn codex_trust_notice_blames_the_space_rather_than_offering_a_pane_that_declines() {
        // Even `Primed`: a trust record earned from a path Codex cannot
        // launch is a leftover for a hook that will never run.
        for state in [
            HookTrustState::Primed,
            HookTrustState::NotPrimed,
            HookTrustState::Unknown,
        ] {
            let notice = codex_trust_notice(state, true, true, false)
                .expect("an unlaunchable path must be reported whatever the trust state");
            assert!(notice.contains("space"), "must name the cause");
            assert!(
                !notice.contains("review pane"),
                "must not offer a pane that can only decline to open"
            );
        }
    }

    // -- agents_ignored_notice -------------------------------------------

    #[test]
    fn agents_ignored_notice_is_none_when_nothing_was_ignored() {
        assert_eq!(
            agents_ignored_notice(&banto_core::config::resolve_agents("")),
            None
        );
        assert_eq!(
            agents_ignored_notice(&banto_core::config::resolve_agents("all")),
            None
        );
        assert_eq!(
            agents_ignored_notice(&banto_core::config::resolve_agents("claude,codex")),
            None
        );
    }

    #[test]
    fn agents_ignored_notice_names_the_dropped_name_and_what_survived() {
        let resolved = banto_core::config::resolve_agents("claude,made-up-product");
        let notice = agents_ignored_notice(&resolved).unwrap();
        assert!(notice.contains("made-up-product"), "{notice}");
        assert!(notice.contains("Claude"), "{notice}");
        // A partial drop, not a total one — must not claim the fallback.
        assert!(!notice.contains("every product"), "{notice}");
    }

    #[test]
    fn agents_ignored_notice_says_it_fell_back_when_nothing_was_recognized() {
        let resolved = banto_core::config::resolve_agents("made-up-product");
        let notice = agents_ignored_notice(&resolved).unwrap();
        assert!(notice.contains("made-up-product"), "{notice}");
        assert!(notice.contains("every product"), "{notice}");
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
        let rows = load_rows(&claude_home, None, &AgeThresholds::default(), &all_agents()).unwrap();
        let titles: Vec<_> = rows.iter().map(|r| r.display_title().to_string()).collect();
        assert_eq!(titles, vec!["New".to_string(), "Old".to_string()]);
    }

    /// Every product this build supports — the "nothing restricted" case
    /// most discovery tests want, so they read as "the usual set" rather
    /// than repeating both variants inline.
    fn all_agents() -> BTreeSet<AgentKind> {
        AgentKind::ALL.into_iter().collect()
    }

    /// The synthetic `threads` shape every Codex test here builds — one
    /// definition rather than a copy per test, because two copies is how
    /// this went wrong once: a column added to the provider's query reached
    /// the copy that existed at the time and not the one a parallel branch
    /// was adding, and the schemas only disagreed after both merged. Never
    /// real session data; a hand-authored shape only.
    const CREATE_THREADS: &str = "\
        CREATE TABLE threads (\
            id TEXT PRIMARY KEY, \
            title TEXT, \
            cwd TEXT, \
            rollout_path TEXT, \
            first_user_message TEXT, \
            updated_at_ms INTEGER, \
            archived INTEGER DEFAULT 0\
        )";

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
        conn.execute_batch(CREATE_THREADS).unwrap();
        conn.execute(
            "INSERT INTO threads (id, title, rollout_path, updated_at_ms) \
             VALUES ('codex-1', 'Codex session', ?1, 0)",
            rusqlite::params![rollout.to_string_lossy()],
        )
        .unwrap();
        drop(conn); // clean close: no -wal left behind for discover() to open around

        let claude_home = ClaudeHome::new(claude_root);
        let metas = discover_all(&claude_home, Some(&codex_home), &all_agents(), None).unwrap();
        let mut ids: Vec<_> = metas.iter().map(|m| m.id.0.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["claude-1".to_string(), "codex-1".to_string()]);
        let agents: Vec<_> = metas.iter().map(|m| m.agent).collect();
        assert!(agents.contains(&AgentKind::ClaudeCode));
        assert!(agents.contains(&AgentKind::Codex));
    }

    #[test]
    fn discover_all_never_calls_the_codex_provider_when_codex_is_disabled() {
        // `state_5.sqlite` is a directory, not a database: if `CodexProvider`
        // were constructed and called at all, opening it would fail and
        // this whole call would return `Err`. Succeeding here is only
        // possible because a disabled product's provider is never reached
        // — the actual property this gate exists for (see `discover_all`'s
        // doc), not just its rows getting dropped afterward.
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
        let codex_home = banto_io::codex_home::CodexHome::new(codex_root.clone());
        std::fs::create_dir_all(codex_home.threads_db_path()).unwrap();

        let claude_home = ClaudeHome::new(claude_root);
        let enabled = BTreeSet::from([AgentKind::ClaudeCode]);
        let metas = discover_all(&claude_home, Some(&codex_home), &enabled, None).unwrap();
        let ids: Vec<_> = metas.iter().map(|m| m.id.0.clone()).collect();
        assert_eq!(ids, vec!["claude-1".to_string()]);
    }

    #[test]
    fn discover_all_never_calls_the_claude_provider_when_claude_is_disabled() {
        // `<claude_home>/projects` is a plain file, not a directory: if
        // `ClaudeCodeProvider` were constructed and called at all,
        // `fs::read_dir` on it would fail and this whole call would return
        // `Err` — the same "never reached, not just filtered" property as
        // the Codex-disabled case above, checked from the other side.
        let dir = tempfile::tempdir().unwrap();
        let claude_root = dir.path().join("claude");
        std::fs::create_dir_all(&claude_root).unwrap();
        std::fs::write(claude_root.join("projects"), "not a directory").unwrap();

        let codex_root = dir.path().join("codex");
        std::fs::create_dir_all(&codex_root).unwrap();
        let rollout = codex_root.join("rollout.jsonl");
        std::fs::write(&rollout, "x").unwrap();
        let codex_home = banto_io::codex_home::CodexHome::new(codex_root);
        let conn = rusqlite::Connection::open(codex_home.threads_db_path()).unwrap();
        conn.execute_batch(CREATE_THREADS).unwrap();
        conn.execute(
            "INSERT INTO threads (id, title, rollout_path, updated_at_ms) \
             VALUES ('codex-1', 'Codex session', ?1, 0)",
            rusqlite::params![rollout.to_string_lossy()],
        )
        .unwrap();
        drop(conn);

        let claude_home = ClaudeHome::new(claude_root);
        let enabled = BTreeSet::from([AgentKind::Codex]);
        let metas = discover_all(&claude_home, Some(&codex_home), &enabled, None).unwrap();
        let ids: Vec<_> = metas.iter().map(|m| m.id.0.clone()).collect();
        assert_eq!(ids, vec!["codex-1".to_string()]);
    }

    #[test]
    fn discover_all_skips_codex_entirely_when_its_home_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let claude_home = ClaudeHome::new(dir.path().to_path_buf());
        let metas = discover_all(&claude_home, None, &all_agents(), None).unwrap();
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
        let rows = rows_from_metas(metas, &claude_home, None, &AgeThresholds::default());

        let ids: Vec<_> = rows.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids, vec!["new".to_string(), "old".to_string()]);
        assert_eq!(rows[0].title.as_deref(), Some("Title new"));
        assert_eq!(rows[0].agent, AgentKind::ClaudeCode);
        assert_eq!(rows[0].size, 42);
    }

    #[test]
    fn a_proven_live_codex_session_is_alive_not_aged() {
        let now = SystemTime::now();
        let mut codex = meta("codex-live", now - Duration::from_secs(SECS_PER_DAY * 30));
        codex.agent = AgentKind::Codex;
        let live_codex = BTreeSet::from(["codex-live".to_string()]);
        assert_eq!(
            activity_from_observations(
                &codex,
                &[],
                &HashSet::new(),
                &live_codex,
                now,
                &AgeThresholds::default(),
            ),
            Activity::Alive
        );
    }

    /// Set a file's mtime without pulling in an extra crate: reopen and write
    /// is not enough for a deterministic value, so use `std::fs` + a manual
    /// `set_modified` via `File::set_modified` (stable since Rust 1.75).
    fn filetime_set(path: &Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    /// A synthetic row with nothing but the fields the loaders' tests read.
    fn row(id: &str, title: &str, cwd: &str, activity: Activity) -> SessionRow {
        SessionRow {
            id: id.into(),
            agent: AgentKind::ClaudeCode,
            title: Some(title.into()),
            cwd: Some(PathBuf::from(cwd)),
            activity,
            is_agent: false,
            preview: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
            archived: false,
        }
    }

    /// A synthetic [`SessionMeta`] fixture with no continuation.
    fn plain_meta(id: &str) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.to_string()),
            agent: AgentKind::ClaudeCode,
            title: None,
            cwd: None,
            source_path: PathBuf::from(format!("{id}.jsonl")),
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            is_agent: false,
            preview: None,
            continuation_of_uuid: None,
            source_archived: false,
        }
    }

    /// Same as [`plain_meta`], but flagged as an auto-compaction
    /// continuation of `parent_uuid`, at a real `source_path` on disk (so
    /// `resolve_lineage`'s streaming scan, invoked through
    /// [`superseded_from_metas`], has a project directory to search).
    fn continuation_meta(id: &str, source_path: PathBuf, parent_uuid: &str) -> SessionMeta {
        SessionMeta {
            continuation_of_uuid: Some(parent_uuid.to_string()),
            source_path,
            ..plain_meta(id)
        }
    }

    #[test]
    fn superseded_from_metas_resolves_a_continuation_from_the_shared_discover_pass() {
        // No independent discover() here: `metas` is handed in directly, the
        // same way every call site now shares one discover() pass between
        // rows and lineage — see `superseded_from_metas`'s doc.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("parent.jsonl"),
            "{\"type\":\"user\",\"uuid\":\"P1\"}\n",
        )
        .unwrap();
        let child_path = dir.path().join("child.jsonl");
        std::fs::write(&child_path, "{\"type\":\"mode\"}\n").unwrap();
        let metas = vec![continuation_meta("child", child_path, "P1")];

        let store = Store::open_in_memory().unwrap();
        let failed = RefCell::new(HashSet::new());

        let superseded = superseded_from_metas(&metas, &store, &failed);

        assert_eq!(superseded, HashSet::from(["parent".to_string()]));
        assert!(failed.borrow().is_empty());
    }

    #[test]
    fn superseded_from_metas_is_empty_for_sessions_without_a_continuation() {
        let store = Store::open_in_memory().unwrap();
        let failed = RefCell::new(HashSet::new());
        let metas = vec![plain_meta("a"), plain_meta("b")];

        let superseded = superseded_from_metas(&metas, &store, &failed);

        assert!(superseded.is_empty());
        assert!(failed.borrow().is_empty());
    }

    #[test]
    fn superseded_from_metas_records_an_unresolvable_continuation_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let child_path = dir.path().join("child.jsonl");
        std::fs::write(&child_path, "{\"type\":\"mode\"}\n").unwrap();
        let metas = vec![continuation_meta(
            "child",
            child_path,
            "nowhere-to-be-found",
        )];

        let store = Store::open_in_memory().unwrap();
        let failed = RefCell::new(HashSet::new());

        let superseded = superseded_from_metas(&metas, &store, &failed);

        assert!(superseded.is_empty());
        assert!(failed.borrow().contains(&SessionId("child".to_string())));
    }

    #[test]
    fn mark_archived_marks_a_row_banto_itself_archived() {
        let store = Store::open_in_memory().unwrap();
        store.archive_session(&SessionId("a".to_string())).unwrap();
        let rows = vec![row("a", "A", "", Activity::Alive)];

        let marked = mark_archived(rows, &store);
        assert_eq!(marked.len(), 1, "banto's own archive keeps the row, marked");
        assert!(marked[0].archived);
    }

    #[test]
    fn mark_archived_still_drops_a_row_only_codex_marked_archived() {
        let store = Store::open_in_memory().unwrap();
        // banto's own archive table has nothing for "a" at all — this row's
        // only reason to be hidden is `source_archived`, set by discovery
        // from Codex's own `threads.archived` (see `provider::codex`).
        let rows = vec![SessionRow {
            source_archived: true,
            archived: false,
            ..row("a", "A", "", Activity::Alive)
        }];

        assert!(mark_archived(rows, &store).is_empty());
    }

    #[test]
    fn mark_archived_leaves_an_unarchived_row_unmarked() {
        let store = Store::open_in_memory().unwrap();
        let rows = vec![row("a", "A", "", Activity::Alive)];

        let marked = mark_archived(rows, &store);
        assert_eq!(marked.len(), 1);
        assert!(!marked[0].archived);
    }

    #[test]
    fn mark_archived_marks_via_bantos_own_archive_even_after_codex_unarchives() {
        // The disagreement case: the operator archived "a" in banto (`d`)
        // independently of Codex; Codex's own `archived` flag has since
        // gone back to false (`codex unarchive`, reflected on the next
        // discovery as `source_archived: false`). The row stays hidden —
        // banto's own archive is a separate fact banto never clears just
        // because the source disagrees; only banto's own unarchive would.
        let store = Store::open_in_memory().unwrap();
        store.archive_session(&SessionId("a".to_string())).unwrap();
        let rows = vec![SessionRow {
            source_archived: false,
            archived: false,
            ..row("a", "A", "", Activity::Alive)
        }];

        let marked = mark_archived(rows, &store);
        assert_eq!(marked.len(), 1);
        assert!(marked[0].archived);
    }
}
