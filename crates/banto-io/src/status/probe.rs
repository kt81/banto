//! PID liveness (and ancestry) behind a mockable trait.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Checks whether a process id refers to a live process, and its parentage.
///
/// Kept behind a trait so unit tests can substitute a mock instead of
/// touching real processes.
pub trait ProcessProbe {
    /// Return `true` if a process with `pid` is currently alive.
    fn is_alive(&self, pid: u32) -> bool;

    /// The parent process id of `pid`, if it could be determined (the
    /// process may no longer exist, or the platform may not expose this).
    /// Used to recognize a brigade member's `claude` process even when it
    /// isn't banto's own direct PTY child (e.g. launched via a cmd/npm
    /// shim) — see [`ancestry_reaches`].
    fn parent_pid(&self, pid: u32) -> Option<u32>;
}

/// [`ProcessProbe`] implementation backed by the `sysinfo` crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct SysinfoProbe;

impl ProcessProbe for SysinfoProbe {
    fn is_alive(&self, pid: u32) -> bool {
        let pid = Pid::from_u32(pid);
        // Refresh only the queried PID with the cheapest refresh kind;
        // never walk the full process table.
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing(),
        );
        system.process(pid).is_some()
    }

    fn parent_pid(&self, pid: u32) -> Option<u32> {
        let pid = Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing(),
        );
        system.process(pid)?.parent().map(Pid::as_u32)
    }
}

/// Whether `live_pid`'s own process ancestry — itself, then its parent, its
/// parent's parent, and so on up to `max_depth` hops — reaches `target_pid`.
/// The multi-hop walk (not just a direct-parent check) is what survives
/// `claude` being launched via a cmd/npm shim, where banto's own PTY child
/// is the shim, not `claude` itself; `max_depth` bounds the walk so a
/// mocked (or genuinely cyclic) probe can never loop forever.
pub fn ancestry_reaches(
    live_pid: u32,
    target_pid: u32,
    probe: &dyn ProcessProbe,
    max_depth: u32,
) -> bool {
    if live_pid == target_pid {
        return true;
    }
    let mut pid = live_pid;
    for _ in 0..max_depth {
        match probe.parent_pid(pid) {
            Some(parent) if parent == target_pid => return true,
            Some(parent) => pid = parent,
            None => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn sysinfo_probe_sees_current_process() {
        let probe = SysinfoProbe;
        assert!(probe.is_alive(std::process::id()));
    }

    /// A probe whose parentage is an explicit `child -> parent` map — no
    /// real process table involved.
    struct MockAncestryProbe(HashMap<u32, u32>);

    impl ProcessProbe for MockAncestryProbe {
        fn is_alive(&self, _pid: u32) -> bool {
            true
        }

        fn parent_pid(&self, pid: u32) -> Option<u32> {
            self.0.get(&pid).copied()
        }
    }

    #[test]
    fn ancestry_reaches_when_the_pids_are_equal() {
        let probe = MockAncestryProbe(HashMap::new());
        assert!(ancestry_reaches(100, 100, &probe, 5));
    }

    #[test]
    fn ancestry_reaches_through_a_one_level_shim() {
        // live_pid (claude) -> target_pid (the shim banto actually spawned).
        let probe = MockAncestryProbe(HashMap::from([(200, 100)]));
        assert!(ancestry_reaches(200, 100, &probe, 5));
    }

    #[test]
    fn ancestry_does_not_reach_past_the_depth_cap() {
        // A chain of exactly 6 hops from 206 down to 200; target 100 sits
        // one hop beyond that. depth 5 walks 206->205->204->203->202->201
        // (5 parent lookups) without ever reaching 200, let alone 100.
        let probe = MockAncestryProbe(HashMap::from([
            (206, 205),
            (205, 204),
            (204, 203),
            (203, 202),
            (202, 201),
            (201, 200),
            (200, 100),
        ]));
        assert!(!ancestry_reaches(206, 100, &probe, 5));
    }

    #[test]
    fn ancestry_does_not_reach_when_the_chain_terminates_first() {
        // 200's parent is unknown (probe returns None) before ever reaching
        // target_pid — a genuine non-match, not just a depth exhaustion.
        let probe = MockAncestryProbe(HashMap::new());
        assert!(!ancestry_reaches(200, 100, &probe, 5));
    }
}
