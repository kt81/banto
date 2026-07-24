//! Pure debounce/merge logic: coalesces bursts of raw per-root changes into
//! a single event, fired only once its quiet period has fully elapsed.
//!
//! No I/O and no real clock: [`Debouncer::poll`] takes `now` as a parameter,
//! so tests drive it with synthetic timestamps instead of sleeping.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use super::WatchRoot;

/// Debounces raw per-root changes.
///
/// [`record`](Debouncer::record) marks a root dirty at a given time;
/// [`poll`](Debouncer::poll) returns (and clears) the roots whose quiet
/// period has fully elapsed as of `now`, coalescing any burst of `record`
/// calls in between into a single result per root.
#[derive(Debug)]
pub struct Debouncer {
    quiet: Duration,
    pending: BTreeMap<WatchRoot, SystemTime>,
}

impl Debouncer {
    /// A debouncer that waits `quiet` after the last change before firing.
    pub fn new(quiet: Duration) -> Self {
        Self {
            quiet,
            pending: BTreeMap::new(),
        }
    }

    /// Record that `root` changed at `at`. Repeated calls for the same root
    /// push its quiet-period deadline out to the latest `at` seen.
    pub fn record(&mut self, root: WatchRoot, at: SystemTime) {
        self.pending
            .entry(root)
            .and_modify(|last| {
                if at > *last {
                    *last = at;
                }
            })
            .or_insert(at);
    }

    /// Roots whose quiet period has elapsed as of `now`, removed from the
    /// pending set so each burst fires exactly once.
    ///
    /// A `now` before the last recorded change (clock moved backward) is
    /// treated as "not yet elapsed" rather than firing early.
    pub fn poll(&mut self, now: SystemTime) -> Vec<WatchRoot> {
        let ready: Vec<WatchRoot> = self
            .pending
            .iter()
            .filter_map(|(&root, &last)| {
                let elapsed = now.duration_since(last).ok()?;
                (elapsed >= self.quiet).then_some(root)
            })
            .collect();
        for root in &ready {
            self.pending.remove(root);
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
    }

    #[test]
    fn does_not_fire_before_the_quiet_period_elapses() {
        let mut d = Debouncer::new(Duration::from_millis(100));
        d.record(WatchRoot::Projects, t(0));
        assert_eq!(d.poll(t(50)), Vec::new());
    }

    #[test]
    fn fires_once_the_quiet_period_elapses() {
        let mut d = Debouncer::new(Duration::from_millis(100));
        d.record(WatchRoot::Projects, t(0));
        assert_eq!(d.poll(t(100)), vec![WatchRoot::Projects]);
    }

    #[test]
    fn a_burst_of_records_resets_the_deadline_to_the_latest() {
        let mut d = Debouncer::new(Duration::from_millis(100));
        d.record(WatchRoot::Projects, t(0));
        d.record(WatchRoot::Projects, t(50)); // pushes the deadline to 150
        assert_eq!(d.poll(t(100)), Vec::new());
        assert_eq!(d.poll(t(150)), vec![WatchRoot::Projects]);
    }

    #[test]
    fn each_root_debounces_independently() {
        let mut d = Debouncer::new(Duration::from_millis(100));
        d.record(WatchRoot::Projects, t(0));
        d.record(WatchRoot::Sessions, t(80));
        assert_eq!(d.poll(t(100)), vec![WatchRoot::Projects]);
        assert_eq!(d.poll(t(180)), vec![WatchRoot::Sessions]);
    }

    #[test]
    fn firing_clears_pending_so_it_never_fires_twice() {
        let mut d = Debouncer::new(Duration::from_millis(100));
        d.record(WatchRoot::Projects, t(0));
        assert_eq!(d.poll(t(200)), vec![WatchRoot::Projects]);
        assert_eq!(d.poll(t(300)), Vec::new());
    }

    #[test]
    fn clock_moving_backward_never_fires_early() {
        let mut d = Debouncer::new(Duration::from_millis(100));
        d.record(WatchRoot::Projects, t(1000));
        assert_eq!(d.poll(t(500)), Vec::new());
    }

    #[test]
    fn no_pending_changes_yields_empty() {
        let mut d = Debouncer::new(Duration::from_millis(100));
        assert_eq!(d.poll(t(1_000_000)), Vec::new());
    }

    #[test]
    fn multiple_roots_ready_at_once_are_all_returned_in_declaration_order() {
        let mut d = Debouncer::new(Duration::from_millis(50));
        d.record(WatchRoot::Sessions, t(0));
        d.record(WatchRoot::Projects, t(0));
        assert_eq!(
            d.poll(t(50)),
            vec![WatchRoot::Projects, WatchRoot::Sessions]
        );
    }
}
