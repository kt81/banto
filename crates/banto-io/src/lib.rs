//! banto-io: the outside world — everything that touches a filesystem,
//! spawns a process, or talks to sqlite. `banto-core` may never depend on
//! this crate (`docs/DISCIPLINE.md` §2); this crate depends on it.
//!
//! Module map:
//! - [`config`]  loading `banto_core::config`'s types from disk
//! - [`opener`]  opening/focusing sessions in a real terminal (psmux, WT)
//! - [`process`] spawning the resumed session's process
//! - [`provider`] session discovery + tolerant JSONL parsing
//! - [`pty`]     PTY host abstraction (portable-pty)
//! - [`status`]  live-session state (the I/O half; bucketing is core's)
//! - [`store`]   sqlite cache, FTS5, groups/pins, session<->pane map
//! - [`watch`]   filesystem watching (notify) for live TUI updates

pub mod config;
pub mod opener;
pub mod process;
pub mod provider;
pub mod pty;
pub mod status;
pub mod store;
pub mod watch;
