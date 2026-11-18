//! The lines of the pane lifecycle log (`panes.log`), as pure values.
//!
//! The UI writes these lines through `banto-io`'s `pane_log`; nothing here reads
//! a clock or touches a file. A line is built only from fixed words, integers,
//! yes/no flags and identifiers that have passed [`token`] or [`id`]. No field
//! takes operator text: a title, a path, an error message, a pane's bytes or a
//! keystroke has no argument to go into. `every_text_carrying_variant_leaves_its_marker_out`
//! holds that property for each Event and Cmd that carries such text.
//!
//! Which Events and Cmds are written is decided by the exhaustive matches in this
//! file, with no wildcard arm, so a new variant does not compile until someone
//! has decided whether it is logged. That decision is recorded in
//! `docs/DISCIPLINE.md` §6.2.

use std::collections::HashMap;
use std::fmt::Display;
use std::hash::Hash;

use crate::engine::{Cmd, Event, GoinkyoLook, GoinkyoRow, SessionKey, StoreIntent};
use crate::model::{AgentKind, BrigadeId, MemberToken, PtyExitReason};

/// No line is longer than this. A line over it loses its trailing fields, whole
/// (see [`Line::render`]). The prefix, the kind, a `key=` and a `by=` are placed
/// first wherever a line has them, and at their longest they come to about 220
/// bytes, so the cut never removes them.
pub const MAX_LINE_BYTES: usize = 300;

/// Lines admitted per window of one minute; see [`RateLimit`].
pub const RATE_LIMIT_PER_MINUTE: u32 = 200;

/// The length of the window [`RateLimit`] counts in. Public so that a test that
/// drives the rate limit by hand does not have to repeat the number.
pub const WINDOW_MS: u128 = 60_000;
const UNKNOWN: &str = "?";
const MAX_TOKEN_CHARS: usize = 40;
const MAX_ID_CHARS: usize = 64;

/// The types a [`Line`] writes as a number. Integers only: a number field is
/// digits, so nothing that could carry operator text can stand in for one.
pub trait Integer: Display {}

macro_rules! integers {
    ($($ty:ty)*) => {$(
        impl Integer for $ty {}
        impl Integer for &$ty {}
    )*};
}
integers!(i8 i16 i32 i64 i128 isize u8 u16 u32 u64 u128 usize);

/// One line of the log, before its time prefix: a kind word and `name=value`
/// fields. Built only through the methods below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    kind: &'static str,
    fields: String,
}

impl Line {
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            fields: String::new(),
        }
    }

    pub fn word(self, name: &'static str, value: &'static str) -> Self {
        self.push(name, value.to_string())
    }

    pub fn num<T: Integer>(self, name: &'static str, value: T) -> Self {
        self.push(name, value.to_string())
    }

    pub fn flag(self, name: &'static str, yes: bool) -> Self {
        self.word(name, if yes { "yes" } else { "no" })
    }

    pub fn token(self, name: &'static str, value: &str) -> Self {
        self.push(name, token(value))
    }

    pub fn key(self, name: &'static str, key: &SessionKey) -> Self {
        self.push(name, id(&key.log_id()))
    }

    /// A session id as the log names every session: through [`SessionKey::log_id`].
    pub fn session(self, name: &'static str, session_id: &str) -> Self {
        self.push(name, id(&SessionKey::from_id(session_id).log_id()))
    }

    fn push(mut self, name: &'static str, value: String) -> Self {
        self.fields.push(' ');
        self.fields.push_str(name);
        self.fields.push('=');
        self.fields.push_str(&value);
        self
    }

    /// The line as written, with its wall clock and its offset from the start of
    /// the UI process. A line over [`MAX_LINE_BYTES`] is cut back to the last
    /// space that fits. No field holds a space, so that drops whole trailing
    /// fields and never part of one.
    pub fn render(&self, wall_ms: u128, since_ms: u128) -> String {
        let mut text = format!("{wall_ms} panes +{since_ms}ms {}{}", self.kind, self.fields);
        if text.len() > MAX_LINE_BYTES {
            let mut end = MAX_LINE_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let cut = text[..end].rfind(' ').unwrap_or(0);
            text.truncate(cut);
        }
        text
    }

    /// The line without its time prefix: what two writes of the same state
    /// compare equal on.
    pub fn body(&self) -> String {
        format!("{}{}", self.kind, self.fields)
    }
}

/// An operator-chosen or externally-produced word, kept only when it is a plain
/// token: letters, digits, `_`, `.`, `-`, at most [`MAX_TOKEN_CHARS`].
fn token(value: &str) -> String {
    let plain = !value.is_empty()
        && value.chars().count() <= MAX_TOKEN_CHARS
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if plain {
        value.to_string()
    } else {
        UNKNOWN.to_string()
    }
}

/// An identifier as a session key's log form may carry it: a token that may also
/// hold `:` (the `new::` and `new-worker::` placeholders), at most
/// [`MAX_ID_CHARS`].
fn id(value: &str) -> String {
    let plain = !value.is_empty()
        && value.chars().count() <= MAX_ID_CHARS
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | ':'));
    if plain {
        value.to_string()
    } else {
        UNKNOWN.to_string()
    }
}

/// What the shell adds to an Event that the Event does not carry: the pane's
/// process id, which only the shell holds, and the facts [`PaneFacts`] reads
/// from state before the Event is folded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneFacts {
    /// Whether an exit of this pane was asked for: a kill the shell sent.
    pub asked: bool,
    pub member: Option<(BrigadeId, MemberToken)>,
    pub pid: Option<u32>,
}

/// What the shell adds to a Cmd: whether the pane's key held a handle before
/// the Cmd ran, and (for an open) whether the open replaced one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CmdFacts {
    pub handle_present: bool,
    pub replaced: bool,
    /// The pid of the handle an open left under its key, for the lines that name it.
    pub pid: Option<u32>,
}

/// The line for the start of the UI process.
pub fn start_line(version: &str, commit: &str, schema: i64, pid: u32) -> Line {
    Line::new("start")
        .token("version", version)
        .token("commit", commit)
        .num("schema", schema)
        .num("pid", pid)
}

/// The Event's variant name, as the `by=` field of the Cmds it produced names it.
/// Exhaustive: a variant added to [`Event`] needs a name here.
pub fn event_name(ev: &Event) -> &'static str {
    match ev {
        Event::Input(_) => "Input",
        Event::Resized { .. } => "Resized",
        Event::PtyOutput { .. } => "PtyOutput",
        Event::PtyExited { .. } => "PtyExited",
        Event::PtyOutputLost { .. } => "PtyOutputLost",
        Event::PtyWriteDropped { .. } => "PtyWriteDropped",
        Event::NewSessionCwdChecked { .. } => "NewSessionCwdChecked",
        Event::Spawned { .. } => "Spawned",
        Event::SpawnFailed { .. } => "SpawnFailed",
        Event::RowsLoaded { .. } => "RowsLoaded",
        Event::DiscoveryResult { .. } => "DiscoveryResult",
        Event::ClaudeWorkerDirectoryUntrusted { .. } => "ClaudeWorkerDirectoryUntrusted",
        Event::ArchiveDone { .. } => "ArchiveDone",
        Event::UnarchiveDone { .. } => "UnarchiveDone",
        Event::GroupJoinDone { .. } => "GroupJoinDone",
        Event::GroupRenameDone { .. } => "GroupRenameDone",
        Event::GroupDeleteDone { .. } => "GroupDeleteDone",
        Event::MembershipResolved { .. } => "MembershipResolved",
        Event::SidebarWidthLoaded { .. } => "SidebarWidthLoaded",
        Event::BellWhenFocusedLoaded { .. } => "BellWhenFocusedLoaded",
        Event::LastWorkerAgentLoaded { .. } => "LastWorkerAgentLoaded",
        Event::StartupNotices { .. } => "StartupNotices",
        Event::CodexTrustChecked { .. } => "CodexTrustChecked",
        Event::CodexCatalogRead { .. } => "CodexCatalogRead",
        Event::BrigadeFormed { .. } => "BrigadeFormed",
        Event::WorkerAdded { .. } => "WorkerAdded",
        Event::Disbanded { .. } => "Disbanded",
        Event::WorkerDismissed { .. } => "WorkerDismissed",
        Event::ExitedWorkerDismissed { .. } => "ExitedWorkerDismissed",
        Event::MemberSessionRecorded { .. } => "MemberSessionRecorded",
        Event::MemberSessionForked { .. } => "MemberSessionForked",
        Event::Tick { .. } => "Tick",
        Event::QuiescenceSampled { .. } => "QuiescenceSampled",
        Event::GoinkyoAwaitingSpawn { .. } => "GoinkyoAwaitingSpawn",
        Event::WindowFocusChanged { .. } => "WindowFocusChanged",
    }
}

/// The pane whose facts the log reads before an Event is folded, for the Events
/// that are about one pane's exit or spawn: the pid and the member, which
/// [`PaneFacts`] carries. Exhaustive, as [`event_name`] is.
pub fn pane_facts_key(ev: &Event) -> Option<&SessionKey> {
    match ev {
        Event::PtyExited { key, .. }
        | Event::PtyOutputLost { key }
        | Event::PtyWriteDropped { key }
        | Event::Spawned { key, .. }
        | Event::SpawnFailed { key, .. } => Some(key),
        Event::Input(_)
        | Event::Resized { .. }
        | Event::PtyOutput { .. }
        | Event::NewSessionCwdChecked { .. }
        | Event::RowsLoaded { .. }
        | Event::DiscoveryResult { .. }
        | Event::ClaudeWorkerDirectoryUntrusted { .. }
        | Event::ArchiveDone { .. }
        | Event::UnarchiveDone { .. }
        | Event::GroupJoinDone { .. }
        | Event::GroupRenameDone { .. }
        | Event::GroupDeleteDone { .. }
        | Event::MembershipResolved { .. }
        | Event::SidebarWidthLoaded { .. }
        | Event::BellWhenFocusedLoaded { .. }
        | Event::LastWorkerAgentLoaded { .. }
        | Event::StartupNotices { .. }
        | Event::CodexTrustChecked { .. }
        | Event::CodexCatalogRead { .. }
        | Event::BrigadeFormed { .. }
        | Event::WorkerAdded { .. }
        | Event::Disbanded { .. }
        | Event::WorkerDismissed { .. }
        | Event::ExitedWorkerDismissed { .. }
        | Event::MemberSessionRecorded { .. }
        | Event::MemberSessionForked { .. }
        | Event::Tick { .. }
        | Event::QuiescenceSampled { .. }
        | Event::GoinkyoAwaitingSpawn { .. }
        | Event::WindowFocusChanged { .. } => None,
    }
}

/// The lines an Event is written as: none for an Event the log does not record,
/// one for most, and one per roster entry for a membership resolution. Nothing
/// is written per tick, per output chunk or per key.
///
/// Exhaustive, as [`event_name`] is.
pub fn event_lines(ev: &Event, facts: &PaneFacts) -> Vec<Line> {
    match ev {
        Event::PtyExited { key, reason } => vec![
            with_member(
                with_pid(Line::new("PtyExited").key("key", key), facts.pid),
                &facts.member,
            )
            .flag("asked", facts.asked)
            .exit(reason.as_ref()),
        ],
        Event::PtyOutputLost { key } => vec![with_member(
            Line::new("PtyOutputLost").key("key", key),
            &facts.member,
        )],
        Event::PtyWriteDropped { key } => vec![with_member(
            Line::new("PtyWriteDropped").key("key", key),
            &facts.member,
        )],
        Event::Spawned { key, agent } => vec![with_pid(
            Line::new("Spawned")
                .key("key", key)
                .word("agent", agent_word(*agent)),
            facts.pid,
        )],
        Event::SpawnFailed {
            key,
            refusal,
            os_error,
            ..
        } => vec![
            Line::new("SpawnFailed")
                .key("key", key)
                .word("refusal", refusal.as_word())
                .os_error(*os_error),
        ],
        Event::DiscoveryResult {
            key,
            session_id,
            member,
        } => vec![with_member(
            Line::new("DiscoveryResult")
                .key("key", key)
                .session("session", session_id),
            member,
        )],
        Event::MemberSessionForked {
            brigade_id,
            token,
            old_id,
            new_id,
        } => vec![
            Line::new("MemberSessionForked")
                .num("brigade", brigade_id)
                .token("member", token)
                .session("old", old_id)
                .session("new", new_id),
        ],
        Event::MembershipResolved {
            session_id,
            membership,
            members,
            ..
        } => {
            let head = Line::new("MembershipResolved").session("session", session_id);
            let head = match membership {
                Some((brigade_id, token, role)) => head
                    .num("brigade", brigade_id)
                    .token("member", token)
                    .word("role", role.as_token()),
                None => head.word("brigade", "none"),
            };
            let mut lines = vec![head.num("roster", members.as_ref().map_or(0, Vec::len))];
            let brigade = membership.as_ref().map(|(brigade_id, _, _)| *brigade_id);
            for (token, role, member_session) in members.iter().flatten() {
                let line = Line::new("MembershipRoster")
                    .token("member", token)
                    .word("role", role.as_token());
                let line = match brigade {
                    Some(brigade_id) => line.num("brigade", brigade_id),
                    None => line.word("brigade", "none"),
                };
                lines.push(match member_session {
                    Some(session_id) => line.session("member_session", session_id),
                    None => line.word("member_session", "none"),
                });
            }
            lines
        }
        Event::BrigadeFormed { result, .. } => {
            let line = Line::new("BrigadeFormed").flag("ok", result.is_ok());
            vec![match result {
                Ok((brigade_id, tokens)) => {
                    line.num("brigade", brigade_id).num("members", tokens.len())
                }
                Err(_) => line.word("brigade", "none"),
            }]
        }
        Event::WorkerAdded {
            brigade_id,
            worker_agent,
            result,
            ..
        } => vec![
            Line::new("WorkerAdded")
                .num("brigade", brigade_id)
                .word("agent", worker_agent.as_token())
                .flag("ok", result.is_ok()),
        ],
        Event::Disbanded { brigade_id, result } => vec![
            Line::new("Disbanded")
                .num("brigade", brigade_id)
                .flag("ok", result.is_ok()),
        ],
        Event::WorkerDismissed { brigade_id, result } => vec![
            Line::new("WorkerDismissed")
                .num("brigade", brigade_id)
                .flag("ok", result.is_ok()),
        ],
        Event::ExitedWorkerDismissed {
            brigade_id,
            token,
            repeated,
            result,
        } => vec![
            Line::new("ExitedWorkerDismissed")
                .num("brigade", brigade_id)
                .token("member", token)
                .flag("repeated", *repeated)
                .flag("ok", result.is_ok()),
        ],
        Event::Input(_)
        | Event::Resized { .. }
        | Event::PtyOutput { .. }
        | Event::NewSessionCwdChecked { .. }
        | Event::RowsLoaded { .. }
        | Event::ClaudeWorkerDirectoryUntrusted { .. }
        | Event::ArchiveDone { .. }
        | Event::UnarchiveDone { .. }
        | Event::GroupJoinDone { .. }
        | Event::GroupRenameDone { .. }
        | Event::GroupDeleteDone { .. }
        | Event::SidebarWidthLoaded { .. }
        | Event::BellWhenFocusedLoaded { .. }
        | Event::LastWorkerAgentLoaded { .. }
        | Event::StartupNotices { .. }
        | Event::CodexTrustChecked { .. }
        | Event::CodexCatalogRead { .. }
        | Event::MemberSessionRecorded { .. }
        | Event::Tick { .. }
        | Event::QuiescenceSampled { .. }
        | Event::GoinkyoAwaitingSpawn { .. }
        | Event::WindowFocusChanged { .. } => Vec::new(),
    }
}

/// The pane a Cmd acts on through its handle, for the Cmds the log writes. Read
/// before the Cmd runs, to say whether the key held a handle.
///
/// Exhaustive, as [`event_name`] is.
pub fn cmd_handle_key(cmd: &Cmd) -> Option<&SessionKey> {
    match cmd {
        Cmd::OpenEmbedded { key, .. } | Cmd::OpenCodexTrustPane { key } | Cmd::KillPty { key } => {
            Some(key)
        }
        Cmd::RekeyPty { from, .. } => Some(from),
        Cmd::Store(_)
        | Cmd::WritePty { .. }
        | Cmd::ResizePty { .. }
        | Cmd::CheckNewSessionCwd { .. }
        | Cmd::CheckCodexTrust
        | Cmd::ReadCodexCatalog { .. }
        | Cmd::SweepDataDir
        | Cmd::ForwardClipboardToHost { .. }
        | Cmd::RingHostBell { .. }
        | Cmd::Reload => None,
    }
}

/// Whether the log writes this Cmd. A store intent is written only when
/// [`store_line`] writes it, so the two cannot disagree.
///
/// Exhaustive, as [`event_name`] is.
pub fn is_logged_cmd(cmd: &Cmd) -> bool {
    match cmd {
        Cmd::OpenEmbedded { .. }
        | Cmd::OpenCodexTrustPane { .. }
        | Cmd::KillPty { .. }
        | Cmd::RekeyPty { .. } => true,
        Cmd::Store(intent) => store_line(intent, "").is_some(),
        Cmd::WritePty { .. }
        | Cmd::ResizePty { .. }
        | Cmd::CheckNewSessionCwd { .. }
        | Cmd::CheckCodexTrust
        | Cmd::ReadCodexCatalog { .. }
        | Cmd::SweepDataDir
        | Cmd::ForwardClipboardToHost { .. }
        | Cmd::RingHostBell { .. }
        | Cmd::Reload => false,
    }
}

/// The line a Cmd is written as, or `None` for a Cmd the log does not record.
/// `by` is the name of the Event whose update produced the Cmd. It follows the
/// key (or the intent) so that a cut line keeps it.
///
/// Exhaustive, as [`event_name`] is.
pub fn cmd_line(cmd: &Cmd, by: &'static str, facts: CmdFacts) -> Option<Line> {
    let line = match cmd {
        Cmd::OpenEmbedded {
            key,
            target,
            brigade,
            ..
        } => {
            let line = Line::new("OpenEmbedded")
                .key("key", key)
                .word("by", by)
                .word("agent", target.agent.as_token())
                .flag("replaced", facts.replaced);
            let line = match brigade {
                Some((brigade_id, token, role)) => line
                    .num("brigade", brigade_id)
                    .token("member", token)
                    .word("role", role.as_token()),
                None => line.word("brigade", "none"),
            };
            if target.id.is_empty() {
                line.word("session", "none")
            } else {
                line.session("session", &target.id)
            }
        }
        Cmd::OpenCodexTrustPane { key } => with_pid(
            Line::new("OpenCodexTrustPane")
                .key("key", key)
                .word("by", by)
                .flag("replaced", facts.replaced),
            facts.pid,
        ),
        Cmd::KillPty { key } => Line::new("KillPty")
            .key("key", key)
            .word("by", by)
            .flag("handle", facts.handle_present),
        Cmd::RekeyPty { from, to } => Line::new("RekeyPty")
            .key("from", from)
            .key("to", to)
            .word("by", by)
            .flag("handle", facts.handle_present),
        Cmd::Store(intent) => return store_line(intent, by),
        Cmd::WritePty { .. }
        | Cmd::ResizePty { .. }
        | Cmd::CheckNewSessionCwd { .. }
        | Cmd::CheckCodexTrust
        | Cmd::ReadCodexCatalog { .. }
        | Cmd::SweepDataDir
        | Cmd::ForwardClipboardToHost { .. }
        | Cmd::RingHostBell { .. }
        | Cmd::Reload => return None,
    };
    Some(line)
}

/// The line for a store intent that sets, clears or removes a member's session
/// or row. Every other intent is not written.
///
/// Exhaustive over [`StoreIntent`].
fn store_line(intent: &StoreIntent, by: &'static str) -> Option<Line> {
    let line = match intent {
        StoreIntent::FormBrigade {
            worker_count,
            worker_agent,
            ..
        } => Line::new("Store")
            .word("intent", "FormBrigade")
            .word("by", by)
            .num("workers", worker_count)
            .word("agent", worker_agent.as_token()),
        StoreIntent::AddWorker {
            brigade_id,
            worker_agent,
            ..
        } => Line::new("Store")
            .word("intent", "AddWorker")
            .word("by", by)
            .num("brigade", brigade_id)
            .word("agent", worker_agent.as_token()),
        StoreIntent::Disband { brigade_id } => Line::new("Store")
            .word("intent", "Disband")
            .word("by", by)
            .num("brigade", brigade_id),
        StoreIntent::DismissWorker { brigade_id, token } => Line::new("Store")
            .word("intent", "DismissWorker")
            .word("by", by)
            .num("brigade", brigade_id)
            .token("member", token),
        StoreIntent::DismissExitedWorker {
            brigade_id,
            token,
            repeated,
        } => Line::new("Store")
            .word("intent", "DismissExitedWorker")
            .word("by", by)
            .num("brigade", brigade_id)
            .token("member", token)
            .flag("repeated", *repeated),
        StoreIntent::SetMemberSession {
            brigade_id,
            token,
            session_id,
        } => Line::new("Store")
            .word("intent", "SetMemberSession")
            .word("by", by)
            .num("brigade", brigade_id)
            .token("member", token)
            .session("session", session_id),
        StoreIntent::ClearMemberSession { brigade_id, token } => Line::new("Store")
            .word("intent", "ClearMemberSession")
            .word("by", by)
            .num("brigade", brigade_id)
            .token("member", token),
        StoreIntent::SetPin { .. }
        | StoreIntent::Archive { .. }
        | StoreIntent::Unarchive { .. }
        | StoreIntent::JoinGroup { .. }
        | StoreIntent::RenameGroup { .. }
        | StoreIntent::DeleteGroup { .. }
        | StoreIntent::ResolveMembership { .. }
        | StoreIntent::SaveBrigadeLayout { .. }
        | StoreIntent::SaveSidebarWidth { .. }
        | StoreIntent::SaveBellWhenFocused { .. }
        | StoreIntent::ClearRequest { .. }
        | StoreIntent::SaveLastWorkerAgent { .. } => return None,
    };
    Some(line)
}

/// The Goinkyo state line of one brigade: the row, the Director, the roster read,
/// the one-shot guard and whether its pane is open or being opened.
///
/// When `roster_read` is `unreadable` the `row` and `director` fields are
/// placeholders, as [`GoinkyoLook`] says.
pub fn goinkyo_line(look: &GoinkyoLook) -> Line {
    // An unreadable roster says nothing about the row or the Director, so the
    // line writes no value for either: it must not claim what was not read.
    let (row, director) = if look.roster_read {
        let row = match &look.row {
            GoinkyoRow::Absent => "absent".to_string(),
            GoinkyoRow::NoSession => "no_session".to_string(),
            GoinkyoRow::Session(session_id) => {
                format!("session:{}", id(&SessionKey::from_id(session_id).log_id()))
            }
        };
        let director = if look.director_hosted {
            "hosted"
        } else {
            "not_hosted"
        };
        (row, director)
    } else {
        ("unread".to_string(), "unknown")
    };
    let guard = match &look.guard.guard {
        None => "none".to_string(),
        Some(key) if key.is_synthetic() => "placeholder".to_string(),
        Some(key) => format!("session:{}", id(&key.log_id())),
    };
    let mut line = Line::new("goinkyo").num("brigade", look.brigade_id);
    line = line.push("row", row);
    line = line.word("director", director);
    line = line.word(
        "roster",
        if look.roster_read {
            "read"
        } else {
            "unreadable"
        },
    );
    line = line.push("guard", guard);
    line.flag("pane", look.guard.pane)
        .flag("opening", look.guard.opening)
}

fn with_pid(line: Line, pid: Option<u32>) -> Line {
    match pid {
        Some(pid) => line.num("pid", pid),
        None => line.word("pid", "none"),
    }
}

fn with_member(line: Line, member: &Option<(BrigadeId, MemberToken)>) -> Line {
    match member {
        Some((brigade_id, token)) => line.num("brigade", brigade_id).token("member", token),
        None => line.word("member", "none"),
    }
}

fn agent_word(agent: Option<AgentKind>) -> &'static str {
    match agent {
        Some(agent) => agent.as_token(),
        None => "none",
    }
}

impl Line {
    fn exit(self, reason: Option<&PtyExitReason>) -> Self {
        match reason {
            Some(PtyExitReason::Code(code)) => self.word("exit", "code").num("code", code),
            Some(PtyExitReason::Signal(name)) => self.word("exit", "signal").token("signal", name),
            Some(PtyExitReason::Unknown) => self.word("exit", "unknown"),
            None => self.word("exit", "none"),
        }
    }

    fn os_error(self, os_error: Option<i32>) -> Self {
        match os_error {
            Some(code) => self.num("os_error", code),
            None => self.word("os_error", "none"),
        }
    }
}

/// The admission rule for lines: at most [`RATE_LIMIT_PER_MINUTE`] per window of
/// a minute, counted from the first line of that window. Lines over the limit
/// are counted and dropped; the count is reported with the next line that is
/// admitted, so a storm costs one extra line, not a flood.
#[derive(Debug, Default)]
pub struct RateLimit {
    window_start_ms: Option<u128>,
    admitted: u32,
    suppressed: u64,
}

/// What the writer does with one line the rule was asked about.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// Write the line. `suppressed` lines were dropped since the last admission
    /// and are reported first when it is non-zero.
    Write {
        suppressed: u64,
    },
    Drop,
}

impl RateLimit {
    pub fn admit(&mut self, now_ms: u128) -> Admission {
        let in_window = self
            .window_start_ms
            .is_some_and(|start| now_ms.saturating_sub(start) < WINDOW_MS);
        if !in_window {
            self.window_start_ms = Some(now_ms);
            self.admitted = 0;
        }
        if self.admitted < RATE_LIMIT_PER_MINUTE {
            self.admitted += 1;
            Admission::Write {
                suppressed: std::mem::take(&mut self.suppressed),
            }
        } else {
            self.suppressed += 1;
            Admission::Drop
        }
    }
}

/// A pane's handle reaped: the pane has no screen left and its process is
/// being closed. The process id is the one the handle held, if it had one.
pub fn reaped_line(key: &SessionKey, pid: Option<u32>) -> Line {
    with_pid(Line::new("Reaped").key("key", key), pid)
}

/// The line that reports lines the rate limit dropped.
pub fn suppressed_line(count: u64) -> Line {
    Line::new("suppressed").num("count", count)
}

/// Keeps the body last written per key, so that a state line is written when it
/// changes and not on every tick that repeats it. A body is remembered only once
/// it has been written: a line the rate limit dropped is still a change next tick.
#[derive(Debug, Default)]
pub struct ChangedOnly<K: Hash + Eq> {
    last: HashMap<K, String>,
}

impl<K: Hash + Eq> ChangedOnly<K> {
    /// Whether `body` differs from the one last remembered for `key`.
    pub fn changed(&self, key: &K, body: &str) -> bool {
        self.last.get(key).is_none_or(|last| last != body)
    }

    /// Records `body` as the state `key` was last written in.
    pub fn remember(&mut self, key: K, body: &str) {
        self.last.insert(key, body.to_string());
    }
}

#[cfg(test)]
#[path = "pane_log_tests.rs"]
mod tests;
