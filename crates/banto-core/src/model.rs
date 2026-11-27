//! Shared domain types — pure data, no I/O, no heavy dependencies. The
//! future pure-core crate's foundation (docs/DISCIPLINE.md §2).

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::display_text::display_name;

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

    /// Stable, storage-facing product token. This deliberately does not use
    /// `label` or serde's enum spelling: changing either presentation or a
    /// Rust variant must not rewrite the durable brigade database format.
    pub fn as_token(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude",
            AgentKind::Codex => "codex",
        }
    }

    /// Parse a durable product token. Unknown values are intentionally not an
    /// error: a newer banto may have written one that this build cannot use.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "claude" => Some(AgentKind::ClaudeCode),
            "codex" => Some(AgentKind::Codex),
            _ => None,
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

#[cfg(test)]
mod agent_kind_tests {
    use super::AgentKind;

    #[test]
    fn durable_agent_tokens_round_trip_and_reject_unknown_values() {
        for agent in AgentKind::ALL {
            assert_eq!(AgentKind::from_token(agent.as_token()), Some(agent));
        }
        assert_eq!(AgentKind::from_token("future-agent"), None);
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
    /// Whether [`Self::title`] is a name its owner gave the session — Claude
    /// Code's `custom-title`, written by the operator's `/rename` — rather than
    /// one derived for it (`ai-title`, the first user message). The one fact
    /// banto's own naming of a Goinkyo's session needs from the provider: its
    /// title replaces a derived one and never a given one. `false` where the
    /// provider cannot tell, and always for Codex (see the Codex provider).
    pub title_is_custom: bool,
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
    /// Whether the session's own product considers it archived — Codex's
    /// `threads.archived` column (set by `codex archive`); Claude Code has
    /// no equivalent yet and always reports `false`. Distinct from banto's
    /// own archive (`Store::archive_session`, bound to `d`): that one is
    /// banto's per-session-id fact, product-neutral and always reversible
    /// from within banto; this one banto can only ever read, never clear —
    /// `~/.codex` stays read-only (see `banto::session::mark_archived` for
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

/// Whether a hosted session is known to be doing nothing — the one answer to
/// "would ending this lose work in flight", asked by the quit confirmation and
/// by the brigade's going-quiet bell.
///
/// Three answers on purpose, and "no information" is one of them, never folded
/// into the others. Both questions this answers fail safe on it (a quit asks; a
/// bell stays silent), so a reader that cannot tell must say so. Deliberately
/// not [`Activity`]: that is what the session list draws and keeps `Alive` for
/// every status it does not name; this one is decided from the product's own
/// report and never guessed toward `Idle`.
///
/// [`Self::Background`] is a refinement of "not idle", not a fourth answer to
/// the question: ask [`Self::is_working`], not `== NotIdle`, wherever the
/// question is whether ending the session loses something. It exists because a
/// tile and the quit prompt say more about this one reason than about the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quiescence {
    /// Reported idle: nothing running, nothing waiting on the operator.
    Idle,
    /// Reported busy, waiting, or anything else that is not the idle report and
    /// is not [`Self::Background`].
    NotIdle,
    /// Not idle for one reason only: the turn has ended and a background command
    /// of the session is still running (Claude's live status `shell`). Never
    /// reported while the session is also busy, waiting on the operator or on a
    /// dialog, or in a status nothing here knows.
    Background,
    /// Nothing usable was read.
    Unknown,
}

impl Quiescence {
    /// Whether ending the session would lose something in flight: not idle for
    /// any reason, background work included. The question the quit count and the
    /// brigade's quiet watch ask, so `Background` holds both exactly as `NotIdle`
    /// does.
    pub fn is_working(self) -> bool {
        matches!(self, Self::NotIdle | Self::Background)
    }

    /// From a probe that answers "idle?" with `None` for "could not tell" (the
    /// Codex rollout's last turn marker is the one such source).
    pub fn from_probe(idle: Option<bool>) -> Self {
        match idle {
            Some(true) => Self::Idle,
            Some(false) => Self::NotIdle,
            None => Self::Unknown,
        }
    }
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
    /// A Claude session that is [`Activity::Waiting`] because a dialog is open
    /// (a picker the operator opened in the pane), as its live file says. It is
    /// still shown and counted as Waiting; only the bell leaves it out, since the
    /// operator caused it and is looking at it. `false` for everything else,
    /// including every other reason a session waits.
    ///
    /// `#[serde(default)]` so a `docs/DISCIPLINE.md` §8 stream recorded before
    /// this field existed still replays: "no dialog" is what such a recording
    /// would have said.
    #[serde(default)]
    pub dialog_open: bool,
    /// True when a spawned agent (subagent / Agent-Teams teammate) ran this
    /// session, rather than the user starting it interactively. See
    /// [`SessionMeta::is_agent`].
    pub is_agent: bool,
    /// True when this session is archived in banto: by the operator (`d`), or by
    /// banto itself for a dismissed Goinkyo's session, on the operator's standing
    /// instruction. The archive itself keeps no record of which of the two did
    /// it; for a Goinkyo's session the store's `consultations` record does (see
    /// `Store::consultation_titles`), and the list does not use it for this. It is
    /// banto's own per-session fact and reversible from within banto (`D`) —
    /// unlike [`Self::source_archived`], which the source product owns and
    /// banto can only ever read. A row carrying this is listed only at
    /// [`crate::app::Reveal::Archived`].
    ///
    /// `#[serde(default)]` for the same reason as `Event::PtyExited::reason`:
    /// a `docs/DISCIPLINE.md` §8 stream recorded before this field existed
    /// still replays, and "not archived" is the honest reading of a recording
    /// made when banto could not say.
    #[serde(default)]
    pub archived: bool,
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
    /// deserializes (same reasoning as `archived` above).
    #[serde(default)]
    pub source_archived: bool,
}

impl SessionRow {
    /// Text used as the search haystack: `title + " " + cwd`.
    pub fn haystack(&self) -> String {
        let title = self.title.as_deref().unwrap_or("");
        format!("{title} {}", self.cwd_display())
    }

    /// The session's title, if it has one the operator could read: `None` when
    /// nothing visible is left of it (empty, whitespace, only control or
    /// invisible characters), the rule `EmporiumState::brigade_name` applies to
    /// the same text. Not folded or sanitised — a caller that puts it on one
    /// line still does that.
    ///
    /// A surface that draws the row shows [`NO_TITLE`] for `None`; a sentence
    /// uses [`Self::sentence_name`]. The id is no title: a session `/clear` has
    /// just moved a pane onto has none until its first message, and a UUID in
    /// the title's place read as the session's name.
    pub fn title(&self) -> Option<&str> {
        self.title
            .as_deref()
            .filter(|title| display_name(title).is_some())
    }

    /// What a sentence calls the session: its title, else the first eight
    /// characters of its id ([`short_id`]), which is enough to tell two apart
    /// and short enough to sit in a status line.
    pub fn sentence_name(&self) -> String {
        self.title()
            .map_or_else(|| short_id(&self.id), str::to_string)
    }

    /// cwd rendered as a string (empty when unknown).
    pub fn cwd_display(&self) -> String {
        self.cwd
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    }
}

/// What a surface that draws a session without a title shows in the title's
/// place: the list row, the Details panel and `banto list`.
pub const NO_TITLE: &str = "(no title)";

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

/// A session about to be opened — pure data the emporium's
/// `Cmd::OpenEmbedded` carries; the actual opening (spawning a PTY child) is
/// `banto` (bin) and `banto_io`'s job.
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
/// `update_member_session_moved` (recognizing a rename as the Goinkyo's
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

/// The Planner's own [`MemberToken`]: fixed, like the Goinkyo's, because a
/// brigade has at most one Planner at a time and it lives exactly as long as
/// the Plan it opened with. As with the Goinkyo's, it coincides with the role's
/// own [`BrigadeRole::as_token`] form by choice and nothing enforces it.
pub const PLANNER_TOKEN: &str = "planner";

/// The sender word of a message banto itself wrote, such as "the operator
/// approved Plan 1 version 2". It sits in the `from_session` column where a
/// member's token would, which is why no member may be named it
/// ([`crate::member_name::looks_like_token`]) and why no reader may look it up
/// among the roster: nothing that carries it came from an agent.
pub const BANTO_SENDER: &str = "banto";

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
    /// files a request (as Claude, or as Codex when `goinkyo_agent` says so —
    /// never opened by the operator directly), addressable only by name,
    /// never a broadcast recipient.
    Goinkyo,
    /// Works a Plan out with the operator, who talks to it directly. Opened by
    /// the Director's `open_planner` (Claude only), addressable only by name and
    /// only by the Director, never a broadcast recipient. Its member row
    /// disappears only in the transaction that closes its Plan.
    Planner,
}

impl BrigadeRole {
    /// The token persisted in the `role` column.
    pub fn as_token(self) -> &'static str {
        match self {
            BrigadeRole::Director => "director",
            BrigadeRole::Worker => "worker",
            BrigadeRole::Goinkyo => "goinkyo",
            BrigadeRole::Planner => "planner",
        }
    }

    /// Parse a persisted `role` token. `None` for anything other than
    /// `"director"`/`"worker"`/`"goinkyo"`/`"planner"` — every writer of this column
    /// goes through [`Self::as_token`], which can only ever produce one of
    /// those, so an unrecognized value means the row did not come from
    /// banto's own write path.
    pub fn from_token(token: &str) -> Option<BrigadeRole> {
        match token {
            "director" => Some(BrigadeRole::Director),
            "worker" => Some(BrigadeRole::Worker),
            "goinkyo" => Some(BrigadeRole::Goinkyo),
            "planner" => Some(BrigadeRole::Planner),
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
            BrigadeRole::Director => &[
                (BrigadeRole::Worker, true),
                (BrigadeRole::Goinkyo, false),
                (BrigadeRole::Planner, false),
            ],
            BrigadeRole::Worker => &[(BrigadeRole::Director, true)],
            BrigadeRole::Goinkyo => &[(BrigadeRole::Director, true)],
            BrigadeRole::Planner => &[(BrigadeRole::Director, true)],
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
    /// The agent banto selected when this member's pane was opened. `None`
    /// preserves older rows, whose product must still be inferred from a
    /// discovered session row when one exists.
    pub agent: Option<AgentKind>,
    /// When `banto _hook` last fired for this member — the hook reached it, which
    /// is not the same as the briefing being printed: a resume whose history
    /// already holds the briefing prints nothing and records this all the same
    /// (`banto_io::codex_briefing`). `None` until the first time it fires
    /// (Claude never sets this at all,
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
    /// What this member's Director calls it, if it has named it
    /// (`banto_core::member_name`). Only what a reader may show: the store
    /// applies the rule when it reads the roster, so a stored value that fails
    /// it, repeats another member's, or is a token arrives here as `None`. The
    /// token stays the identifier; this is a label.
    pub display_name: Option<String>,
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
    /// What the gate did for this message (`interrupt_worker`), when it refused
    /// tool calls of the recipient: `Gated` or `Capped`, and `None` for every
    /// other word and for none. The inbox header says it, because the transcript
    /// records a refused call as a hook's error and nothing in it says the call
    /// never ran or whose decision it was. A message pulled before any call was
    /// refused carries no header.
    pub interrupted: Option<InterruptOutcome>,
}

/// What one `check_messages` call took out of a member's unread mail: the
/// messages it delivers, oldest first, and how many more are still unread behind
/// them. The member's cursor is past the first and short of the second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxPage {
    pub messages: Vec<BrigadeMessage>,
    pub remaining: usize,
}

/// A brigade message reduced to what a collapsed tile shows: when it was
/// queued and the first line of its body (see
/// `banto_io::store::glance_line` for the reduction). Message text in a plain
/// data struct, so it travels in an `Event` and shows up in a recorded
/// stream (`BANTO_RECORD_EVENTS`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageGlance {
    pub created_at_ms: i64,
    pub line: String,
    /// The member token of whoever queued the message (`from_session` holds a
    /// token despite its name). `None` in a capture recorded before this
    /// field existed.
    #[serde(default)]
    pub from_token: Option<MemberToken>,
}

/// One member's arrangement preferences as the store keeps them (its columns
/// in `brigade_members`): its place in the stack, its row weight, whether it
/// is collapsed. `None`/`false` mean "no preference".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMemberLayout {
    pub token: MemberToken,
    /// Position in the stack order, 0 first. Members are ordered by this, then
    /// by token for equal ranks.
    pub stack_rank: Option<u32>,
    pub row_weight: Option<u16>,
    pub minimized: bool,
}

/// A brigade's stored arrangement preferences: the brigade-level values (each
/// `None` meaning "the default") and one entry per member that has any.
/// Plain data for the store boundary — it travels in `Event`s and
/// `StoreIntent`s, so it is serde — and converted to and from the live
/// `layout::BrigadeLayout` in core.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredLayout {
    pub master_permille: Option<u16>,
    pub collapsed_stack_width: Option<u16>,
    pub solo_member: Option<MemberToken>,
    pub members: Vec<StoredMemberLayout>,
}

/// What a collapsed tile shows of a member's mail, as the shell read it from
/// the store this tick: the newest message addressed to the member and the
/// newest it sent (either may not exist).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberGlance {
    pub inbound: Option<MessageGlance>,
    pub sent: Option<MessageGlance>,
}

/// Why a hosted child process exited, as far as banto could tell — plain
/// data, translated at the shell boundary from the PTY host's own richer
/// exit status (`banto_io::pty::PtyIo::exited`, `Child::wait()`'s own
/// `portable_pty::ExitStatus`) since this crate cannot depend on the I/O
/// crate that type lives in (docs/DISCIPLINE.md §2). A live wait error sends
/// `Unknown` as a nonterminal notification and is retried; only a status
/// confirms exit. Historical `PtyExited` events with `Unknown` retain their
/// recorded meaning on replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PtyExitReason {
    /// The child ran to completion with this exit code (0 is success).
    Code(u32),
    /// The child was terminated by this named signal. Unix only —
    /// `portable_pty::ExitStatus` never reports one on Windows.
    Signal(String),
    /// A wait failed without confirming an exit, or a historical recorded
    /// exit had no known reason.
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

/// What banto did about one `interrupt_worker` request, as the store keeps it in
/// `brigade_messages.interrupt_outcome` and the tool prints it.
///
/// Two processes write it. The UI makes the first decision, over an undecided
/// row only, with one of the first five words (it can see what is hosted and the
/// configuration, which the Worker's `_mcp` cannot). The Worker's own `_mcp`
/// writes the other two, from what the gate it answers did; the gate itself never
/// reads the word. [`Self::replaceable`] is the one place that says which word
/// may be written over which.
///
/// The tokens are the durable format: changing a variant's spelling must not
/// rewrite what a store already holds, so the strings are written out here and
/// nowhere derived. A token this build does not know (an older banto wrote
/// `pressed`, `stopped` and the like when it typed an Esc) reads as no outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterruptOutcome {
    /// The Worker had already read the message when the request was first seen.
    Read,
    /// The Worker is a Codex one: its message goes as ordinary mail.
    Codex,
    /// `[brigade] relay = "manual"`: the Worker has no gate, and the message goes
    /// as ordinary mail.
    RelayManual,
    /// No hosted pane with a live process (the reason is an [`InterruptWhy`]).
    NoPane,
    /// A hosted Claude Worker with the relay on: its gate refuses its next tool
    /// calls until it reads the message. Nothing has been refused yet.
    Armed,
    /// The gate has refused a call of the Worker's. The call never ran.
    Gated,
    /// The gate let the main agent's call through after refusing its quota, so
    /// the request ends with the message still unread.
    Capped,
}

impl InterruptOutcome {
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Codex => "codex",
            Self::RelayManual => "relay-manual",
            Self::NoPane => "no-pane",
            Self::Armed => "armed",
            Self::Gated => "gated",
            Self::Capped => "capped",
        }
    }

    /// Unknown values are not an error: a newer banto may have written one this
    /// build does not know.
    pub fn from_token(token: &str) -> Option<Self> {
        Some(match token {
            "read" => Self::Read,
            "codex" => Self::Codex,
            "relay-manual" => Self::RelayManual,
            "no-pane" => Self::NoPane,
            "armed" => Self::Armed,
            "gated" => Self::Gated,
            "capped" => Self::Capped,
            _ => return None,
        })
    }

    /// The words this one may be written over, besides no word at all, which
    /// every outcome may be written over. The one table of the overwrite rule:
    /// the store builds its guard from it and nothing else spells the rule.
    ///
    /// The UI's words go over nothing, so a repeated or late decision is a no-op.
    /// `gated` goes over `armed` and also over no word, because the gate can
    /// refuse a call before the UI's write has landed. `capped` goes over what
    /// `gated` does and `gated` too, and nothing goes over `capped`.
    pub fn replaceable(self) -> &'static [InterruptOutcome] {
        match self {
            Self::Read | Self::Codex | Self::RelayManual | Self::NoPane | Self::Armed => &[],
            Self::Gated => &[Self::Armed],
            Self::Capped => &[Self::Armed, Self::Gated],
        }
    }

    /// Whether the gate refused a call for this request, which is what the
    /// Worker's inbox header says.
    pub fn gate_refused(self) -> bool {
        matches!(self, Self::Gated | Self::Capped)
    }
}

/// Why a decision that folds several reasons into one outcome chose it. Only the
/// pane log keeps it, as one fixed word: the column stays the outcome alone, and
/// nothing a pane showed is ever recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterruptWhy {
    /// `no-pane`: the member's brigade is being closed.
    Closing,
    /// `no-pane`: the pane is being ended.
    Ending,
    /// `no-pane`: the pane is open but cannot be read yet: it has no session or
    /// its product is not resolved.
    Starting,
    /// `no-pane`: nothing hosts the member.
    NotRunning,
}

impl InterruptWhy {
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Closing => "closing",
            Self::Ending => "ending",
            Self::Starting => "starting",
            Self::NotRunning => "not-running",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const ALL_OUTCOMES: [InterruptOutcome; 7] = [
        InterruptOutcome::Read,
        InterruptOutcome::Codex,
        InterruptOutcome::RelayManual,
        InterruptOutcome::NoPane,
        InterruptOutcome::Armed,
        InterruptOutcome::Gated,
        InterruptOutcome::Capped,
    ];

    #[test]
    fn interrupt_outcome_tokens_round_trip_and_unknown_is_none() {
        for outcome in ALL_OUTCOMES {
            assert_eq!(
                InterruptOutcome::from_token(outcome.as_token()),
                Some(outcome)
            );
        }
        // What an older banto wrote when it typed an Esc is a word nobody reads.
        for retired in [
            "pressed",
            "not-mid-turn",
            "waiting",
            "expired",
            "stopped",
            "other-text",
            "still-busy",
            "from-the-future",
        ] {
            assert_eq!(InterruptOutcome::from_token(retired), None, "{retired}");
        }
    }

    #[test]
    fn the_overwrite_table_lets_only_the_workers_own_words_replace_another() {
        use InterruptOutcome::*;
        let table: Vec<(InterruptOutcome, Vec<InterruptOutcome>)> = ALL_OUTCOMES
            .into_iter()
            .map(|outcome| (outcome, outcome.replaceable().to_vec()))
            .collect();
        assert_eq!(
            table,
            [
                (Read, vec![]),
                (Codex, vec![]),
                (RelayManual, vec![]),
                (NoPane, vec![]),
                (Armed, vec![]),
                (Gated, vec![Armed]),
                (Capped, vec![Armed, Gated]),
            ]
        );
    }

    #[test]
    fn only_gated_and_capped_say_a_call_was_refused() {
        let refused: Vec<_> = ALL_OUTCOMES
            .into_iter()
            .filter(|outcome| outcome.gate_refused())
            .collect();
        assert_eq!(refused, [InterruptOutcome::Gated, InterruptOutcome::Capped]);
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

    fn row_titled(id: &str, title: Option<&str>) -> SessionRow {
        SessionRow {
            id: id.into(),
            agent: AgentKind::ClaudeCode,
            title: title.map(str::to_string),
            cwd: None,
            dialog_open: false,
            activity: Activity::Alive,
            is_agent: false,
            preview: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
            archived: false,
        }
    }

    #[test]
    fn a_title_is_only_a_title_when_something_visible_is_left_of_it() {
        let id = "0123456789abcdef";
        assert_eq!(row_titled(id, Some("Fix login")).title(), Some("Fix login"));
        // Not folded or sanitised: the caller still does that.
        assert_eq!(row_titled(id, Some(" a  b ")).title(), Some(" a  b "));
        for none in [
            None,
            Some(""),
            Some("   \n\t"),
            Some("\u{1b}\u{7}"),
            Some("\u{200b}\u{200d}"),
        ] {
            assert_eq!(row_titled(id, none).title(), None, "{none:?}");
        }
    }

    #[test]
    fn a_sentence_calls_an_untitled_session_by_the_start_of_its_id() {
        let id = "0123456789abcdef-0123";
        assert_eq!(
            row_titled(id, Some("Fix login")).sentence_name(),
            "Fix login"
        );
        assert_eq!(row_titled(id, None).sentence_name(), "01234567");
        assert_eq!(row_titled(id, Some("  ")).sentence_name(), "01234567");
        assert_eq!(row_titled("short", None).sentence_name(), "short");
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
