//! Shared domain types.
//!
//! This file is owned by the team lead. Module agents must not edit it;
//! propose changes in your final report instead.

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
