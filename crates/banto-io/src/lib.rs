//! banto-io: the outside world — everything that touches a filesystem,
//! spawns a process, or talks to sqlite. `banto-core` may never depend on
//! this crate (`docs/DISCIPLINE.md` §2); this crate depends on it.
//!
//! Module map:
//! - [`claude_home`] the root Claude Code stores its state under, plus its subdirs
//! - [`codex_home`] the root the Codex CLI stores its state under: threads/logs db, rollout tree
//! - [`codex_liveness`] Codex session liveness via `logs_2.sqlite` (the Codex-side double-resume guard)
//! - [`config`]  loading `banto_core::config`'s types from disk
//! - [`lineage`] resolving auto-compaction parent links (beside the provider)
//! - [`opener`]  opening/focusing sessions in a real terminal (psmux, WT)
//! - [`process`] spawning the resumed session's process
//! - [`provider`] session discovery + tolerant JSONL parsing (Claude Code) / sqlite (Codex)
//! - [`pty`]     PTY host abstraction (portable-pty)
//! - [`sqlite_ro`] read-only access to a Codex sqlite database Codex may be actively writing
//! - [`status`]  live-session state (the I/O half; bucketing is core's) — Claude Code only, deliberately
//! - [`store`]   sqlite: groups/pins/archived, brigades, session<->pane map
//! - [`watch`]   filesystem watching (notify) for live TUI updates

pub mod claude_home;
pub mod codex_home;
pub mod codex_liveness;
pub mod config;
pub mod lineage;
pub mod opener;
pub mod process;
pub mod provider;
pub mod pty;
pub(crate) mod sqlite_ro;
pub mod status;
pub mod store;
pub mod watch;
