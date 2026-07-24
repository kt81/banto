//! Filesystem watching for live TUI updates (phase 3).
//!
//! Watches `<claude_home>/projects/` and `<claude_home>/sessions/` with
//! `notify`, debouncing bursts into coarse per-root change events:
//! - [`debounce`] holds the debounce/merge logic as a pure function of
//!   recorded timestamps and an injected `now`; fully unit-tested without a
//!   real clock or filesystem.
//! - [`source`] wraps the real `notify` watcher behind [`ChangeSource`], so
//!   callers (and their tests) depend on the trait rather than on `notify`
//!   directly.
//!
//! A typical caller polls once per UI tick: drain raw changes from a
//! [`ChangeSource`] into a [`Debouncer`], then call [`Debouncer::poll`] with
//! the current time to get the roots that should trigger a re-sync.

mod debounce;
mod source;

pub use debounce::Debouncer;
pub use source::{ChangeSource, NotifyChangeSource, WatchError};

use std::time::SystemTime;

/// Which watched root a change was observed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WatchRoot {
    /// `<claude_home>/projects/`: session `.jsonl` files (provider discovery).
    Projects,
    /// `<claude_home>/sessions/`: live-state `<pid>.json` files (activity).
    Sessions,
}

/// One raw, un-debounced change observed under a [`WatchRoot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawChange {
    pub root: WatchRoot,
    pub at: SystemTime,
}
