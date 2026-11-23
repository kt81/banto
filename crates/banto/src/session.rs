//! Session rows for the CLI/TUI: discovery + activity classification.
//!
//! This module wires `banto-io`'s provider/status and `banto-core`'s config
//! modules together into a flat [`SessionRow`] list that both the `list`
//! subcommand and the TUI render. It performs read-only work only; nothing
//! here writes to disk. Everything under `claude_home` is treated as
//! strictly read-only.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, SystemTime};

use banto_core::config::ActivityConfig;
use banto_core::consultation;
pub use banto_core::model::SessionRow;
use banto_core::model::{Activity, AgeBucket, AgentKind, SessionMeta};
use banto_core::status::{AgeThresholds, age_bucket};
use banto_io::claude_home::ClaudeHome;
use banto_io::codex_home::CodexHome;
use banto_io::codex_liveness::live_candidate_thread_ids;
use banto_io::codex_trust::HookTrustState;
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

/// Every notice the status line carries at startup, in the order it carries
/// them: `trust_notice` (already decided, since it needs the store and the
/// agent's own home), then `boot_notices` (decided at startup, where the
/// locations were resolved, the database claimed and the config read). A config
/// value banto cannot use is not one of them: it stops the start instead
/// (`crate::startup`). Pulled out of `crate::embedded::run` so that a notice
/// dropped from the line is a failing test rather than a silent one.
pub fn startup_notices(trust_notice: Option<String>, boot_notices: Vec<String>) -> Vec<String> {
    trust_notice.into_iter().chain(boot_notices).collect()
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

/// The pids of `live` whose process is still the session that wrote its file:
/// what the list, the quit question, the brigade watch and the relay all read as
/// "this Claude session is running". A file left by a process that died, whose
/// pid has since gone to an unrelated one, is not among them
/// ([`SysinfoProbe::alive_live_pids`]). The seam for display and activity policy;
/// the open/resume guard asks `status::entry_is_alive` per entry instead.
pub(crate) fn alive_claude_pids(live: &[banto_io::status::LiveSession]) -> HashSet<u32> {
    SysinfoProbe::alive_live_pids(live)
}

/// The conversion half of [`load_rows`] — classify and sort already-
/// discovered `metas` into [`SessionRow`]s — extracted so a caller that has
/// its own discover() pass doesn't need a second one just for this.
/// `claude_home`/`thresholds` are still needed here: they drive the
/// live-status read and activity classification, neither of which discovery
/// itself touches.
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
    let live_claude = alive_claude_pids(&live);
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
            // Told apart here, from the same live file and liveness pass that
            // gave the activity: the list still draws such a row Waiting, and
            // the bell leaves it out.
            let dialog_open = activity == Activity::Waiting
                && meta.agent == AgentKind::ClaudeCode
                && status::waiting_on_a_dialog(&live, &meta.id.0, &live_claude);
            SessionRow {
                id: meta.id.0,
                agent: meta.agent,
                title: meta.title,
                cwd: meta.cwd,
                dialog_open,
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

/// Names the sessions of ended consultations: each one the store holds a title
/// for is listed as "Goinkyo: <title>" ([`consultation::list_title`]) instead of
/// whatever the provider derived — banto's own kickoff sentence, or Claude's
/// generated title, which read alike for every consultation.
///
/// Precedence is the operator's `/rename` first
/// (`SessionMeta::title_is_custom`, left alone), then banto's title, then what
/// the provider derived (`ai-title`, the first user message). It works on the
/// metas, before [`rows_from_metas`], because that flag does not survive into a
/// [`SessionRow`], whose `title` is already the winner of the provider's own
/// precedence.
///
/// Reads `consultations`, not `archived`: `D` deletes only the archived row, so a
/// restored session keeps its name. A store read failure is tolerated, like
/// [`mark_archived`]'s: the sessions keep the titles the provider gave them.
pub(crate) fn apply_consultation_titles(metas: &mut [SessionMeta], store: &Store) {
    let titles: HashMap<String, String> = store
        .consultation_titles()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, title)| (id.0, title))
        .collect();
    if titles.is_empty() {
        return;
    }
    for meta in metas.iter_mut().filter(|meta| !meta.title_is_custom) {
        if let Some(title) = titles.get(&meta.id.0) {
            meta.title = Some(consultation::list_title(title));
        }
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
            title_is_custom: false,
            cwd: None,
            source_path: PathBuf::from(format!("{id}.jsonl")),
            mtime,
            size: 42,
            is_agent: false,
            preview: None,
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
            dialog_open: false,
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
            dialog_open: false,
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
    fn an_untitled_row_has_no_title_and_is_called_by_its_id() {
        let row = SessionRow {
            id: "the-id".into(),
            agent: AgentKind::ClaudeCode,
            title: None,
            cwd: None,
            dialog_open: false,
            activity: Activity::Alive,
            is_agent: false,
            preview: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
            archived: false,
        };
        assert_eq!(row.title(), None);
        assert_eq!(row.sentence_name(), "the-id");
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

    #[test]
    fn startup_notices_carry_the_boot_notices_right_after_the_trust_notice() {
        let notices = startup_notices(
            Some("trust".to_string()),
            vec!["database".to_string(), "instance".to_string()],
        );
        assert_eq!(notices, ["trust", "database", "instance"]);
    }

    #[test]
    fn startup_notices_are_empty_when_there_is_nothing_to_say() {
        assert!(startup_notices(None, Vec::new()).is_empty());
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
        let titles: Vec<_> = rows.iter().map(SessionRow::sentence_name).collect();
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
    fn a_waiting_session_is_marked_dialog_open_only_for_exactly_that_reason() {
        // The live file's process is this test's own, which is alive.
        let pid = std::process::id();
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let claude_home = ClaudeHome::new(dir.path().to_path_buf());
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        for (live_file, activity, dialog_open) in [
            (
                r#"{"status":"waiting","waitingFor":"dialog open"}"#,
                Activity::Waiting,
                true,
            ),
            (
                r#"{"status":"waiting","waitingFor":"input needed"}"#,
                Activity::Waiting,
                false,
            ),
            (
                r#"{"status":"waiting","waitingFor":"a reason nobody has seen"}"#,
                Activity::Waiting,
                false,
            ),
            (
                r#"{"status":"waiting","waitingFor":7}"#,
                Activity::Waiting,
                false,
            ),
            (r#"{"status":"waiting"}"#, Activity::Waiting, false),
            // The reason means nothing beside any other status.
            (
                r#"{"status":"busy","waitingFor":"dialog open"}"#,
                Activity::Busy,
                false,
            ),
            (
                r#"{"status":"idle","waitingFor":"dialog open"}"#,
                Activity::Alive,
                false,
            ),
        ] {
            let body = format!(
                r#"{{"pid":{pid},"sessionId":"sess","kind":"interactive",{}"#,
                &live_file[1..]
            );
            std::fs::write(sessions.join(format!("{pid}.json")), body).unwrap();
            let rows = rows_from_metas(
                vec![meta("sess", when)],
                &claude_home,
                None,
                &AgeThresholds::default(),
            );
            assert_eq!(
                (rows[0].activity, rows[0].dialog_open),
                (activity, dialog_open),
                "{live_file}"
            );
        }
    }

    /// Pins the wiring: the list and the alive set must go through the start-time
    /// check, not bare pid liveness. This test's own pid is alive, so only a
    /// start time that is not the process's own (`u64::MAX`, which no kernel
    /// reports) can tell the file's writer is gone. Where the platform cannot
    /// read a start time (macOS) the pid alone decides, so there is nothing to pin.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_leftover_on_a_recycled_pid_is_not_a_running_session_in_the_list() {
        let pid = std::process::id();
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let claude_home = ClaudeHome::new(dir.path().to_path_buf());
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let row_of = |proc_start: &str| {
            let body = format!(
                r#"{{"pid":{pid},"sessionId":"sess","kind":"interactive","status":"busy"{proc_start}}}"#
            );
            std::fs::write(sessions.join(format!("{pid}.json")), body).unwrap();
            let live = status::read_live_sessions(&claude_home.sessions_dir());
            let alive = alive_claude_pids(&live);
            let rows = rows_from_metas(
                vec![meta("sess", when)],
                &claude_home,
                None,
                &AgeThresholds::default(),
            );
            (alive.contains(&pid), rows[0].activity)
        };

        let (alive, activity) = row_of(r#","procStart":"18446744073709551615""#);
        assert!(!alive, "a pid in use by someone else is not the writer");
        assert!(matches!(activity, Activity::Idle(_)), "{activity:?}");
        // The control: no recorded start leaves the pid to decide, and it is alive.
        let (alive, activity) = row_of("");
        assert!(alive);
        assert_eq!(activity, Activity::Busy);
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
            dialog_open: false,
            activity,
            is_agent: false,
            preview: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
            archived: false,
        }
    }

    fn store_with_ended_consultation(session: &str, title: &str) -> Store {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store.add_goinkyo_member(br, title).unwrap();
        store
            .set_member_session(br, "goinkyo", &SessionId(session.to_string()))
            .unwrap();
        store.dismiss_worker(br, "goinkyo").unwrap();
        store
    }

    fn titled(id: &str, title: Option<&str>, is_custom: bool) -> SessionMeta {
        SessionMeta {
            title: title.map(str::to_string),
            title_is_custom: is_custom,
            ..meta(id, SystemTime::UNIX_EPOCH)
        }
    }

    #[test]
    fn an_ended_consultation_is_listed_under_its_title_behind_the_label() {
        let store = store_with_ended_consultation("g1", "Layout drag");
        // A derived title (Claude's ai-title or the first user line: the provider
        // has already folded them into one string) is replaced; a session nobody
        // consulted is left alone.
        let mut metas = vec![
            titled("g1", Some("Banto brigade Goinkyo pane"), false),
            titled("other", Some("Fix the parser"), false),
        ];

        apply_consultation_titles(&mut metas, &store);

        assert_eq!(metas[0].title.as_deref(), Some("Goinkyo: Layout drag"));
        assert_eq!(metas[1].title.as_deref(), Some("Fix the parser"));
    }

    #[test]
    fn a_title_is_given_to_a_session_that_has_none() {
        let store = store_with_ended_consultation("g1", "Layout drag");
        let mut metas = vec![titled("g1", None, false)];

        apply_consultation_titles(&mut metas, &store);

        assert_eq!(metas[0].title.as_deref(), Some("Goinkyo: Layout drag"));
    }

    #[test]
    fn the_operators_own_rename_wins_over_the_consultations_title() {
        let store = store_with_ended_consultation("g1", "Layout drag");
        let mut metas = vec![titled("g1", Some("my own name"), true)];

        apply_consultation_titles(&mut metas, &store);

        assert_eq!(metas[0].title.as_deref(), Some("my own name"));
    }

    #[test]
    fn the_name_survives_restoring_the_session_with_d() {
        let store = store_with_ended_consultation("g1", "Layout drag");
        store
            .unarchive_session(&SessionId("g1".to_string()))
            .unwrap();
        let mut metas = vec![titled("g1", Some("Banto brigade Goinkyo pane"), false)];

        apply_consultation_titles(&mut metas, &store);
        let rows = mark_archived(
            metas
                .into_iter()
                .map(|meta| SessionRow {
                    title: meta.title,
                    ..row(&meta.id.0, "", "", Activity::Alive)
                })
                .collect(),
            &store,
        );

        assert!(!rows[0].archived, "restored");
        assert_eq!(rows[0].title.as_deref(), Some("Goinkyo: Layout drag"));
    }

    #[test]
    fn with_no_consultations_every_title_is_untouched() {
        let store = Store::open_in_memory().unwrap();
        let mut metas = vec![titled("a", Some("A"), false), titled("b", None, false)];

        apply_consultation_titles(&mut metas, &store);

        assert_eq!(metas[0].title.as_deref(), Some("A"));
        assert_eq!(metas[1].title, None);
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
