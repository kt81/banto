//! Session discovery and parsing.
//!
//! [`SessionProvider`] abstracts the agent product whose sessions we index.
//! The only implementation for now is Claude Code ([`claude_code`]).

pub mod claude_code;

use banto_core::model::SessionMeta;

/// A source of sessions (Claude Code today, other agents later).
pub trait SessionProvider {
    /// Stable provider name stored in the DB, e.g. `"claude-code"`.
    fn name(&self) -> &'static str;

    /// Enumerate all sessions under this provider.
    ///
    /// Tolerant by contract: unreadable files, unknown record types and
    /// malformed lines are skipped, never turned into errors. Only a
    /// completely inaccessible provider root is an error.
    fn discover(&self) -> Result<Vec<SessionMeta>, ProviderError>;
}

/// Errors that abort discovery entirely (per-file problems are skipped).
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider root not accessible: {0}")]
    Io(#[from] std::io::Error),
}
