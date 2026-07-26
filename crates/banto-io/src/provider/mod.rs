//! Session discovery and parsing.
//!
//! [`SessionProvider`] abstracts the agent product whose sessions we index.
//! The only implementation for now is Claude Code ([`claude_code`]).

pub mod claude_code;

use std::path::Path;
use std::time::SystemTime;

use banto_core::model::{AgentKind, SessionId, SessionMeta};

/// A source of sessions (Claude Code today, other agents later).
pub trait SessionProvider {
    /// Which [`AgentKind`] this provider discovers sessions for.
    fn name(&self) -> AgentKind;

    /// Enumerate all sessions under this provider.
    ///
    /// Tolerant by contract: unreadable files, unknown record types and
    /// malformed lines are skipped, never turned into errors. Only a
    /// completely inaccessible provider root is an error.
    fn discover(&self) -> Result<Vec<SessionMeta>, ProviderError>;

    /// Find every session this provider created or updated for `cwd` at or
    /// after `since`, sorted oldest-first by mtime (ties broken by id).
    ///
    /// Used to discover the session id(s) assigned to a `claude` process
    /// that was launched without a known session id (e.g. banto's "new
    /// session" flow, or several auto-spawned brigade Workers landing in the
    /// same cwd at once). Matching is done on the `cwd` recorded *inside*
    /// each candidate file's head record, not by decoding the project
    /// directory name: Claude's cwd-to-directory-name encoding is lossy
    /// (`:`, `\`, `/`, and `.` all collapse to `-`), so a renamed project can
    /// leave files with different recorded cwd values in the same directory.
    ///
    /// `since` is compared against each file's filesystem mtime, which can
    /// have coarse resolution; callers should capture `since` a moment
    /// *before* launching the new session so a file created in the same
    /// clock tick is not missed.
    ///
    /// Read-only: unreadable or broken files are skipped, and a missing
    /// `projects` directory yields an empty result.
    ///
    /// Callers needing to disambiguate a batch (several new sessions
    /// launched into the same cwd around the same instant) should fetch
    /// every match once via this method and assign candidates to pending
    /// entries themselves, excluding ids already claimed elsewhere — see
    /// [`Self::find_new_session`] for the single-best case.
    fn find_new_sessions(&self, cwd: &Path, since: SystemTime) -> Vec<SessionId>;

    /// Find the single most-recently created/updated session for `cwd` at or
    /// after `since` — the newest element of [`Self::find_new_sessions`], so
    /// a tie (identical mtime) resolves the same deterministic way as there:
    /// by id, never by filesystem iteration order.
    ///
    /// Only ever reports one match, which collides when several new sessions
    /// are launched into the *same* cwd around the same instant (e.g. an
    /// emporium brigade auto-spawning several fresh Workers in the
    /// Director's cwd at once) — each caller's independent call would then
    /// resolve to the identical newest file, double-assigning it to more
    /// than one pending session. Callers needing to disambiguate a batch
    /// should call [`Self::find_new_sessions`] directly instead.
    fn find_new_session(&self, cwd: &Path, since: SystemTime) -> Option<SessionId> {
        self.find_new_sessions(cwd, since).pop()
    }
}

/// Errors that abort discovery entirely (per-file problems are skipped).
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider root not accessible: {0}")]
    Io(#[from] std::io::Error),
}
