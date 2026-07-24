//! Shared domain types — pure data, no I/O, no heavy dependencies. The
//! future pure-core crate's foundation (docs/DISCIPLINE.md §2).

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

/// Opaque session identifier (a UUID string for Claude Code).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Metadata for one discovered session, provider-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    pub id: SessionId,
    /// Stable provider name, e.g. "claude-code".
    pub provider: String,
    /// Best-effort title: custom-title > ai-title > first user message.
    pub title: Option<String>,
    /// Working directory the session ran in, if it could be determined.
    pub cwd: Option<PathBuf>,
    /// Path to the session's source file (.jsonl for Claude Code).
    pub source_path: PathBuf,
    /// Last modification time of the source file.
    pub mtime: SystemTime,
    /// Source file size in bytes.
    pub size: u64,
    /// True when this session was run by a spawned agent (subagent /
    /// Agent-Teams teammate) rather than started interactively by the user.
    /// Detected from a `{"type":"agent-setting"}` record in the file head
    /// (observed 2026-07-19); interactive sessions start with `mode` records.
    pub is_agent: bool,
    /// Short single-line excerpt of the first user message, for the summary
    /// panel. Independent of `title` (which may come from custom/ai titles).
    pub preview: Option<String>,
}

/// Activity state rendered as the colored dot in the session list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// A live process reports status=busy for this session.
    Busy,
    /// A live process exists for this session but is idle.
    Alive,
    /// No live process; bucketed by source-file mtime.
    Idle(AgeBucket),
}

/// Age buckets for sessions without a live process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgeBucket {
    Today,
    ThisWeek,
    Older,
}

/// One session as shown in the list: its metadata plus computed activity.
/// The bin crate's `session` module owns discovering these
/// (`session::load_rows`) and the pure formatting built on top of them
/// (`session::short_id`/`humanize_age`/`humanize_size`/`activity_tag`); the
/// struct and its own field-only accessors live here so `banto-core`'s
/// pure core (the emporium's `update`, in particular) can hold and read a
/// row without depending on the bin crate.
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
    /// True when a spawned agent (subagent / Agent-Teams teammate) ran this
    /// session, rather than the user starting it interactively. See
    /// [`SessionMeta::is_agent`].
    pub is_agent: bool,
    /// Short single-line excerpt of the first user message, for the summary
    /// panel. See [`SessionMeta::preview`].
    pub preview: Option<String>,
    /// Last modification time of the session's source file, for the summary
    /// panel's relative-age display (`session::humanize_age`).
    pub mtime: SystemTime,
    /// Source file size in bytes, for the summary panel (`session::humanize_size`).
    pub size: u64,
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

/// Row id of a brigade (sqlite AUTOINCREMENT primary key).
pub type BrigadeId = i64;

/// A banto-owned member identity within a brigade: `"director"` or
/// `"worker-1"`, `"worker-2"`, etc. Stable for the member's lifetime in the
/// brigade, unlike its Claude session id (unknown for a Worker until
/// discovered, and never reused across brigades).
pub type MemberToken = String;

/// A member's role within a brigade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrigadeRole {
    /// Commands the brigade; the user's only interface into it.
    Director,
    /// Carries out the Director's instructions.
    Worker,
}

impl BrigadeRole {
    /// The token persisted in the `role` column.
    pub(crate) fn as_token(self) -> &'static str {
        match self {
            BrigadeRole::Director => "director",
            BrigadeRole::Worker => "worker",
        }
    }

    /// Parse a persisted `role` token leniently: anything other than
    /// `"director"` is treated as a Worker.
    pub(crate) fn from_token(token: &str) -> BrigadeRole {
        if token == "director" {
            BrigadeRole::Director
        } else {
            BrigadeRole::Worker
        }
    }
}

/// A brigade row. Its live membership is loaded separately via
/// `Store::brigade_members`, mirroring groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brigade {
    pub id: BrigadeId,
    pub name: String,
}

/// One member of a brigade: its banto-owned token, role, and Claude session
/// id once known (`None` for a Worker banto has spawned but Claude hasn't
/// assigned an id to yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrigadeMember {
    pub token: MemberToken,
    pub role: BrigadeRole,
    pub claude_session_id: Option<SessionId>,
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
