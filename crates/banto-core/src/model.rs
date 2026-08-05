//! Shared domain types — pure data, no I/O, no heavy dependencies. The
//! future pure-core crate's foundation (docs/DISCIPLINE.md §2).

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Opaque session identifier (a UUID string for Claude Code).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which agent product a session belongs to — the axis this exists to
/// distinguish, which is why it is an enum rather than the bare string it
/// replaced. `SessionProvider` (`banto-io`) keeps its own name: a provider
/// *provides* sessions, this says *whose*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
}

impl AgentKind {
    /// Every agent product banto currently discovers, in display order —
    /// what an absent, empty, or `"all"` `agents` config setting resolves to
    /// (see `config::resolve_agents`). [`Self::toggle`] already hardcodes
    /// "exactly two"; update both the day a third provider lands.
    pub const ALL: [AgentKind; 2] = [AgentKind::ClaudeCode, AgentKind::Codex];

    /// Short product name for display (the row list's agent column, the
    /// new-session modal) — never abbreviated or iconified: no single emoji
    /// names an agent product (see `banto_tui::view`'s module doc's "Emoji
    /// markers" section for why the row markers stay icons while this stays
    /// text).
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "Claude",
            AgentKind::Codex => "Codex",
        }
    }

    /// The other product — the new-session modal's only choice, since there
    /// are exactly two.
    pub fn toggle(self) -> Self {
        match self {
            AgentKind::ClaudeCode => AgentKind::Codex,
            AgentKind::Codex => AgentKind::ClaudeCode,
        }
    }
}

/// Metadata for one discovered session, provider-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    pub id: SessionId,
    /// See [`AgentKind`].
    pub agent: AgentKind,
    /// Best-effort title: custom-title > ai-title > first user message.
    pub title: Option<String>,
    /// Working directory the session ran in, if it could be determined.
    pub cwd: Option<PathBuf>,
    /// Path to the session's source file: the `.jsonl` for Claude Code, the
    /// rollout file for Codex.
    pub source_path: PathBuf,
    /// When this session was last touched — not the same *kind* of fact for
    /// every product: a filesystem mtime for Claude Code (the source file's
    /// own), an application-reported timestamp for Codex
    /// (`threads.updated_at_ms`, whatever Codex itself considers last
    /// activity). Not reconciled to a common meaning here.
    pub mtime: SystemTime,
    /// Source file size in bytes.
    pub size: u64,
    /// Whether this session was run by a spawned agent (Claude Code's
    /// subagent / Agent-Teams teammate) rather than a human at the
    /// keyboard — narrower than the field's own name suggests: "no signal
    /// either way" must default to `false` (assume a human was there, keep
    /// the session visible), never to `true`. Claude Code sets this from a
    /// `{"type":"agent-setting"}` record in the file head (observed
    /// 2026-07-19; interactive sessions start with `mode` records instead).
    /// Codex always reports `false` — it has no equivalent signal yet.
    pub is_agent: bool,
    /// Short single-line excerpt of the first user message, for the summary
    /// panel. Independent of `title` (which may come from custom/ai titles).
    pub preview: Option<String>,
    /// The `logicalParentUuid` of the first `compact_boundary` record found
    /// in the file head, if this session is an auto-compaction continuation
    /// of another one. `None` for an ordinary session (including a manually
    /// `/compact`-ed one, which keeps its original id rather than forking).
    pub continuation_of_uuid: Option<String>,
    /// Whether the session's own product considers it archived — Codex's
    /// `threads.archived` column (set by `codex archive`); Claude Code has
    /// no equivalent yet and always reports `false`. Distinct from banto's
    /// own archive (`Store::archive_session`, bound to `d`): that one is
    /// banto's per-session-id fact, product-neutral and always reversible
    /// from within banto; this one banto can only ever read, never clear —
    /// `~/.codex` stays read-only (see `banto::tui::exclude_archived` for
    /// how the two combine into what the list actually hides).
    pub source_archived: bool,
}

/// Activity state rendered as the colored dot in the session list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Activity {
    /// A live process reports status=busy for this session.
    Busy,
    /// A live process reports that this session is waiting for a human.
    ///
    /// Only Claude Code currently reports this state.
    Waiting,
    /// A live process exists for this session but is idle.
    Alive,
    /// No live process; bucketed by source-file mtime.
    Idle(AgeBucket),
}

/// Age buckets for sessions without a live process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgeBucket {
    Today,
    ThisWeek,
    Older,
}

/// One session as shown in the list: its metadata plus computed activity.
/// `banto::session` owns discovering these (`session::load_rows`, which
/// needs `banto_io`'s provider/status); the plain-text `session::activity_tag`
/// also stays there (the `list` subcommand's formatting, not consumed by
/// either UI). The struct, its own field-only accessors, and the numeric
/// formatting helpers below live here instead so both UI crates
/// (`banto-core`'s own `engine`/`app`, and `banto-tui`'s `view`) can hold,
/// read, and display a row without depending on the bin crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRow {
    /// Session id (UUID for Claude Code).
    pub id: String,
    /// Which agent product this session belongs to. See [`SessionMeta::agent`].
    pub agent: AgentKind,
    /// Best-effort title (`None` when it could not be extracted).
    pub title: Option<String>,
    /// Working directory the session ran in, if known.
    pub cwd: Option<PathBuf>,
    /// Activity state driving the colored dot.
    pub activity: Activity,
    /// True when a spawned agent (subagent / Agent-Teams teammate) ran this
    /// session, rather than the user starting it interactively. See
    /// [`SessionMeta::is_agent`].
    pub is_agent: bool,
    /// Short single-line excerpt of the first user message, for the summary
    /// panel. See [`SessionMeta::preview`].
    pub preview: Option<String>,
    /// Last modification time of the session's source file, for the summary
    /// panel's relative-age display ([`humanize_age`]).
    pub mtime: SystemTime,
    /// Source file size in bytes, for the summary panel ([`humanize_size`]).
    pub size: u64,
    /// See [`SessionMeta::source_archived`]. `#[serde(default)]` so a
    /// record/replay stream captured before this field existed still
    /// deserializes (same reasoning as `Event::RowsLoaded::superseded`).
    #[serde(default)]
    pub source_archived: bool,
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

/// Number of seconds in one hour.
const SECS_PER_HOUR: u64 = 60 * 60;
/// Number of seconds in one day.
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;

/// First 8 characters of a session id (a UUID), for compact display in the
/// summary panel. Char-based (not byte-slicing) so it never panics
/// regardless of what the id turns out to contain.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Format how long ago `mtime` was, relative to `now`, as a short human
/// string for the summary panel: "just now", "5m ago", "3h ago", "2d ago",
/// "3w ago". `mtime` in the future (clock skew, a freshly touched file)
/// reads as "just now" rather than underflowing.
pub fn humanize_age(mtime: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(mtime).unwrap_or_default().as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < SECS_PER_HOUR {
        format!("{}m ago", secs / 60)
    } else if secs < SECS_PER_DAY {
        format!("{}h ago", secs / SECS_PER_HOUR)
    } else if secs < SECS_PER_DAY * 7 {
        format!("{}d ago", secs / SECS_PER_DAY)
    } else {
        format!("{}w ago", secs / (SECS_PER_DAY * 7))
    }
}

/// Compact form of [`humanize_age`] for the list row's right-aligned age
/// column: same bucket boundaries, no " ago" suffix — "now", "5m", "3h",
/// "2d", "3w". `mtime` in the future reads "now", same rationale as
/// `humanize_age`.
pub fn humanize_age_compact(mtime: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(mtime).unwrap_or_default().as_secs();
    if secs < 60 {
        "now".to_string()
    } else if secs < SECS_PER_HOUR {
        format!("{}m", secs / 60)
    } else if secs < SECS_PER_DAY {
        format!("{}h", secs / SECS_PER_HOUR)
    } else if secs < SECS_PER_DAY * 7 {
        format!("{}d", secs / SECS_PER_DAY)
    } else {
        format!("{}w", secs / (SECS_PER_DAY * 7))
    }
}

/// Format a byte count as a short human string for the summary panel:
/// "512 B", "12 KB", "3.4 MB". Whole units only below MB, one decimal at MB
/// and above — session files are small enough that GB never comes up.
pub fn humanize_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

/// A session about to be opened or focused — pure data the emporium's
/// `Cmd::OpenEmbedded` carries; the actual opening (spawning a PTY child,
/// or in the chōba list, resuming/focusing it in a real terminal backend)
/// is `banto` (bin) and `banto_io`'s job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionToOpen {
    pub id: String,
    /// Which agent product to launch. See [`SessionMeta::agent`].
    pub agent: AgentKind,
    pub title: String,
    pub cwd: PathBuf,
}

/// Row id of a brigade (sqlite AUTOINCREMENT primary key).
pub type BrigadeId = i64;

/// A banto-owned member identity within a brigade: `"director"` or
/// `"worker-1"`, `"worker-2"`, etc. Stable for the member's lifetime in the
/// brigade, unlike its session id (unknown for a Worker until discovered,
/// and never reused across brigades).
pub type MemberToken = String;

/// The Director's own [`MemberToken`]. This is deliberately distinct from
/// [`BrigadeRole::Director`]'s [`BrigadeRole::as_token`] role-column string,
/// even though both currently spell `"director"`: member identity is what
/// formation and launches carry, while a role is a separate classification.
pub const DIRECTOR_TOKEN: &str = "director";

/// The Goinkyo's own [`MemberToken`] — a fixed value, unlike a Worker's
/// numbered one, since a brigade only ever has at most one Goinkyo at a
/// time. A different concept from `BrigadeRole::Goinkyo`'s own
/// [`BrigadeRole::as_token`] form (`"goinkyo"` too, but the *role's* string
/// form, read from/written to the `role` column) — they only coincide
/// because this token was chosen to read the same way; nothing enforces
/// that they must match, the same way a Worker's own token (`"worker-1"`)
/// and its role's string form (`"worker"`) already don't.
///
/// Minted once, here, and used everywhere a member-token comparison or
/// construction needs "the Goinkyo, specifically" rather than "whichever
/// role this row has": `mcp::tool_consult_goinkyo` (where the token is
/// actually chosen), `mcp::tool_dismiss_goinkyo`,
/// `engine::update_goinkyo_awaiting_spawn` (both the synthetic key and the
/// `Cmd::OpenEmbedded` member tuple), and `engine::update_discovery_result`/
/// `update_member_session_forked` (recognizing a rename as the Goinkyo's
/// own, to keep `EmporiumState::goinkyo_pane` pointed at the right pane).
/// Before this constant, all six hardcoded the literal independently;
/// letting even one of them drift from the rest would have broken
/// `dismiss_goinkyo` (silently — `Store::dismiss_worker` no-ops on an
/// unknown token), the spawn/discovery/pane-close chain (wrong token
/// embedded in the synthetic key or the launch's `--member`), or the
/// rename-following that keeps `goinkyo_pane` from going stale — each in a
/// different way, none of them raising an error. A pane's own *display*
/// title happens to also read `"goinkyo"` at spawn time
/// (`update_goinkyo_awaiting_spawn`'s `SessionToOpen.title`) but is a third,
/// unrelated concept — cosmetic text nothing reads back or compares — and
/// is left as its own literal rather than reusing this constant.
pub const GOINKYO_TOKEN: &str = "goinkyo";

/// A member's role within a brigade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrigadeRole {
    /// Commands the brigade; the user's only interface into it.
    Director,
    /// Carries out the Director's instructions.
    Worker,
    /// The retired elder called back in to arbitrate a Director/Worker
    /// disagreement or an impasse — advises, never takes the work over.
    /// Auto-spawned by banto itself once a Director's `consult_goinkyo`
    /// files a request (Claude only — see `BrigadeConfig::goinkyo_model` —
    /// never opened by the operator directly), addressable only by name,
    /// never a broadcast recipient.
    Goinkyo,
}

impl BrigadeRole {
    /// The token persisted in the `role` column.
    pub fn as_token(self) -> &'static str {
        match self {
            BrigadeRole::Director => "director",
            BrigadeRole::Worker => "worker",
            BrigadeRole::Goinkyo => "goinkyo",
        }
    }

    /// Parse a persisted `role` token. `None` for anything other than
    /// `"director"`/`"worker"`/`"goinkyo"` — every writer of this column
    /// goes through [`Self::as_token`], which can only ever produce one of
    /// those, so an unrecognized value means the row did not come from
    /// banto's own write path.
    pub fn from_token(token: &str) -> Option<BrigadeRole> {
        match token {
            "director" => Some(BrigadeRole::Director),
            "worker" => Some(BrigadeRole::Worker),
            "goinkyo" => Some(BrigadeRole::Goinkyo),
            _ => None,
        }
    }

    /// The single source of truth for banto's Director/Worker/Goinkyo
    /// messaging rules: every role `self` can reach at all, paired with
    /// whether that role is reached by broadcast (`to` omitted) or only by
    /// naming a member of it. An exhaustive `match`, not a bare table, on
    /// purpose — the guardrail a fourth role has to force open lives in
    /// this one function being a `match`, not in what its arms return; a
    /// bare constant table could grow a new role's row forgotten and no
    /// compiler would say so. Everyone who cares which roles a role can
    /// reach — `banto`'s `mcp::tool_brigade_status` (the roster shown to a
    /// member), `mcp::validate_target` (what `to` may name), and
    /// `briefing::peers_of` (`{peers}` in a launch briefing) — derives it
    /// from here rather than re-deciding it independently, which is what
    /// let the roster and the routing drift apart the first time a third
    /// role existed to disagree about.
    pub fn addressability(self) -> &'static [(BrigadeRole, bool)] {
        match self {
            BrigadeRole::Director => &[(BrigadeRole::Worker, true), (BrigadeRole::Goinkyo, false)],
            BrigadeRole::Worker => &[(BrigadeRole::Director, true)],
            BrigadeRole::Goinkyo => &[(BrigadeRole::Director, true)],
        }
    }

    /// Whether `self` can reach `target` at all — by broadcast, by name, or
    /// both. See [`Self::addressability`].
    pub fn can_reach(self, target: BrigadeRole) -> bool {
        self.addressability()
            .iter()
            .any(|(role, _)| *role == target)
    }

    /// The role `self`'s broadcast (`to` omitted in `send_to_peer`) reaches.
    /// See [`Self::addressability`].
    pub fn broadcast_target(self) -> BrigadeRole {
        self.addressability()
            .iter()
            .find_map(|(role, is_broadcast)| is_broadcast.then_some(*role))
            .expect("every BrigadeRole's addressability table names exactly one broadcast target")
    }
}

/// A brigade row. Its live membership is loaded separately via
/// `Store::brigade_members`, mirroring groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brigade {
    pub id: BrigadeId,
    pub name: String,
}

/// One member of a brigade: its banto-owned token, role, and session id once
/// known (`None` for a Worker banto has spawned but its agent product hasn't
/// assigned an id to yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrigadeMember {
    pub token: MemberToken,
    pub role: BrigadeRole,
    pub session_id: Option<SessionId>,
    /// When `banto _hook` last delivered this member its role briefing —
    /// `None` until the first time it fires (Claude never sets this at all,
    /// it takes its briefing on launch argv instead; see
    /// `crate::model::AgentKind` — a Codex member with this still `None`
    /// after its session has clearly started is the signal that one of the
    /// silent-failure paths `docs/notes/codex-briefing-spike.md` measured
    /// (deferred tool, unapproved MCP call, untrusted hook) ate the
    /// briefing, since every one of those fails without an error banto
    /// could otherwise observe.
    pub briefed_at: Option<SystemTime>,
    /// The session id `banto _hook` was actually running under when it last
    /// recorded [`Self::briefed_at`] — not necessarily [`Self::session_id`]
    /// (a session-id rekey after discovery, or a hook firing for a session
    /// this member's row hasn't caught up to yet, can make them disagree
    /// momentarily). An observation, not an identity: nothing is
    /// unique-indexed on it the way `session_id` is.
    pub briefed_session_id: Option<SessionId>,
}

/// A queued message from one brigade member to the peer role, or to one
/// specific member of it (see the `brigade_messages` migration and the v8
/// `to_member` column): what a recipient pulls via
/// `Store::fetch_brigade_messages`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrigadeMessage {
    /// Monotonic queue id (also the per-member read cursor).
    pub id: i64,
    /// The token of the member that sent it (for attribution in the
    /// firewall framing, e.g. "director" or "worker-1").
    pub from_token: MemberToken,
    pub body: String,
    /// The specific member this was addressed to, if any. `None` means a
    /// broadcast to every member of the recipient role — the original,
    /// still-default addressing.
    pub to_member: Option<MemberToken>,
}

/// Why a hosted child process exited, as far as banto could tell — plain
/// data, translated at the shell boundary from the PTY host's own richer
/// exit status (`banto_io::pty::PtyIo::exited`, `Child::wait()`'s own
/// `portable_pty::ExitStatus`) since this crate cannot depend on the I/O
/// crate that type lives in (docs/DISCIPLINE.md §2). `Unknown` is `wait()`
/// itself failing, not merely "no observation yet" — that case has no
/// `PtyExitReason` at all (see `Event::PtyExited::reason`'s own doc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PtyExitReason {
    /// The child ran to completion with this exit code (0 is success).
    Code(u32),
    /// The child was terminated by this named signal. Unix only —
    /// `portable_pty::ExitStatus` never reports one on Windows.
    Signal(String),
    /// The child is known to have exited, but `wait()` on it returned an
    /// error rather than a status.
    Unknown,
}

impl PtyExitReason {
    /// A short, lowercase, unpunctuated fragment fit for the middle of a
    /// status sentence (`format!("session ended: {title} — {}", reason.describe())`).
    pub fn describe(&self) -> String {
        match self {
            PtyExitReason::Code(0) => "exited normally".to_string(),
            PtyExitReason::Code(code) => format!("exited with code {code}"),
            PtyExitReason::Signal(signal) => format!("terminated by {signal}"),
            PtyExitReason::Unknown => "exit reason unknown".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

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
    fn short_id_takes_the_first_eight_chars() {
        assert_eq!(short_id("0123456789abcdef"), "01234567");
        assert_eq!(short_id("short"), "short");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn humanize_age_covers_every_bucket() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let ago = |secs: u64| now - Duration::from_secs(secs);

        assert_eq!(humanize_age(now, now), "just now");
        assert_eq!(humanize_age(ago(30), now), "just now");
        assert_eq!(humanize_age(ago(5 * 60), now), "5m ago");
        assert_eq!(humanize_age(ago(3 * SECS_PER_HOUR), now), "3h ago");
        assert_eq!(humanize_age(ago(2 * SECS_PER_DAY), now), "2d ago");
        assert_eq!(humanize_age(ago(20 * SECS_PER_DAY), now), "2w ago");
    }

    #[test]
    fn humanize_age_treats_a_future_mtime_as_just_now() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let future = now + Duration::from_secs(500);
        assert_eq!(humanize_age(future, now), "just now");
    }

    #[test]
    fn humanize_age_compact_covers_every_bucket() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let ago = |secs: u64| now - Duration::from_secs(secs);

        assert_eq!(humanize_age_compact(now, now), "now");
        assert_eq!(humanize_age_compact(ago(30), now), "now");
        assert_eq!(humanize_age_compact(ago(5 * 60), now), "5m");
        assert_eq!(humanize_age_compact(ago(3 * SECS_PER_HOUR), now), "3h");
        assert_eq!(humanize_age_compact(ago(2 * SECS_PER_DAY), now), "2d");
        assert_eq!(humanize_age_compact(ago(20 * SECS_PER_DAY), now), "2w");
    }

    #[test]
    fn humanize_age_compact_treats_a_future_mtime_as_now() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let future = now + Duration::from_secs(500);
        assert_eq!(humanize_age_compact(future, now), "now");
    }

    #[test]
    fn humanize_size_covers_every_unit() {
        assert_eq!(humanize_size(512), "512 B");
        assert_eq!(humanize_size(12 * 1024), "12 KB");
        assert_eq!(humanize_size(3 * 1024 * 1024 + 512 * 1024), "3.5 MB");
    }

    #[test]
    fn brigade_role_token_roundtrips_every_variant() {
        for role in [
            BrigadeRole::Director,
            BrigadeRole::Worker,
            BrigadeRole::Goinkyo,
        ] {
            assert_eq!(BrigadeRole::from_token(role.as_token()), Some(role));
        }
    }

    #[test]
    fn brigade_role_from_token_rejects_an_unrecognized_string() {
        assert_eq!(BrigadeRole::from_token("sous-chef"), None);
    }
}
