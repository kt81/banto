//! The live-reload signal: filesystem changes under the watched roots, debounced
//! into "reload now".

use std::time::{Duration, SystemTime};

use crate::claude_home::ClaudeHome;
use crate::codex_home::CodexHome;

use super::{ChangeSource, Debouncer, NotifyChangeSource};

/// How long `projects/`/`sessions/` must stay quiet before a burst of
/// filesystem changes triggers a reload (see [`Debouncer`]).
const DEBOUNCE_QUIET: Duration = Duration::from_millis(250);

/// Watches `claude_home` for changes and debounces them into a "reload now"
/// signal. Construction failures (e.g. an exotic filesystem `notify` can't
/// watch) degrade to "no live updates" rather than blocking the emporium from
/// starting at all.
pub struct LiveWatch {
    source: Option<NotifyChangeSource>,
    debouncer: Debouncer,
}

impl LiveWatch {
    pub fn new(claude_home: &ClaudeHome, codex_home: Option<&CodexHome>) -> Self {
        Self {
            source: NotifyChangeSource::new(claude_home, codex_home).ok(),
            debouncer: Debouncer::new(DEBOUNCE_QUIET),
        }
    }

    /// Drain any pending filesystem changes and report whether their quiet
    /// period has elapsed as of `now`, i.e. whether a reload is due.
    pub fn poll_ready(&mut self, now: SystemTime) -> bool {
        let Some(source) = &self.source else {
            return false;
        };
        for change in source.drain() {
            self.debouncer.record(change.root, change.at);
        }
        !self.debouncer.poll(now).is_empty()
    }
}
