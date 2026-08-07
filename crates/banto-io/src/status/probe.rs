//! PID liveness (and ancestry) behind a mockable trait.

use std::collections::HashSet;

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
    /// isn't banto's own direct PTY child — see [`ancestry_reaches`].
    fn parent_pid(&self, pid: u32) -> Option<u32>;

    /// True if `pid` is alive **and**, when identity can be established, it
    /// is provably the same process that recorded `proc_start` — the
    /// `procStart` field Claude Code itself writes to
    /// `sessions/<pid>.json` (see `crate::status::live::LiveSession`).
    ///
    /// A bare [`Self::is_alive`] only answers "does some process own this
    /// pid right now", which a pid recycled after a crash/kill can answer
    /// `true` for even though it is a completely unrelated process — a risk
    /// this crate's own audit flagged as more likely on Linux/WSL, which
    /// recycles pids far faster than Windows typically does. This method
    /// closes that gap where possible.
    ///
    /// Falls back to [`Self::is_alive`] whenever identity can't be
    /// established: `proc_start` fails to parse, or this implementation has
    /// no reliable way to read the compared-against process's own start
    /// time on the current platform. Declining to strengthen the check is
    /// always safe; a wrong mismatch is not — see
    /// [`SysinfoProbe::is_alive_matching`]'s doc comment for what is and
    /// isn't verified.
    fn is_alive_matching(&self, pid: u32, proc_start: &str) -> bool;
}

/// [`ProcessProbe`] implementation backed by the `sysinfo` crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct SysinfoProbe;

impl SysinfoProbe {
    /// Snapshot which of `pids` are alive through one `sysinfo::System`
    /// refresh.  This belongs outside [`ProcessProbe`]: it serves a list
    /// pass that already knows every PID, while the trait deliberately stays
    /// a small mockable point-query interface for open/resume and ancestry.
    pub fn alive_pids(pids: impl IntoIterator<Item = u32>) -> HashSet<u32> {
        let pids = pids
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .map(Pid::from_u32)
            .collect::<Vec<_>>();
        if pids.is_empty() {
            return HashSet::new();
        }
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing(),
        );
        pids.into_iter()
            .filter(|pid| system.process(*pid).is_some())
            .map(Pid::as_u32)
            .collect()
    }
}

impl ProcessProbe for SysinfoProbe {
    fn is_alive(&self, pid: u32) -> bool {
        Self::alive_pids([pid]).contains(&pid)
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

    /// Verified only on Linux (which WSL2 counts as: it has a real kernel
    /// `/proc`), by comparing `proc_start` against the kernel's own
    /// `starttime` field for `pid` (`/proc/<pid>/stat`, field 22 — clock
    /// ticks since boot, per `man 5 proc`). This is the exact quantity
    /// Claude Code's `procStart` records: confirmed by direct, on-device
    /// comparison of live `sessions/<pid>.json` files against
    /// `/proc/<pid>/stat` (identical values, same units, same reference
    /// point) — not assumed.
    ///
    /// Deliberately does *not* go through `sysinfo`'s own
    /// `Process::start_time()`: that method only returns whole *seconds
    /// since the Unix epoch* (`boot_time + raw_ticks / CLK_TCK`), and
    /// neither `boot_time` nor `CLK_TCK` (needed to convert `proc_start`'s
    /// raw ticks into the same units for a lossless comparison) is part of
    /// its public API — reconstructing them would mean hardcoding an
    /// assumed `CLK_TCK` (typically 100 on Linux, but not guaranteed, and
    /// not verifiable through this crate), turning an exact match into a
    /// guess. Reading the raw ticks directly instead needs no such
    /// assumption: both sides are the same unconverted integer.
    ///
    /// On any other platform, or if `pid`'s `/proc/<pid>/stat` can't be
    /// read/parsed, or `proc_start` itself doesn't parse, falls back to
    /// [`Self::is_alive`] — see the trait doc comment for why that fallback
    /// is the safe direction to fail in.
    fn is_alive_matching(&self, pid: u32, proc_start: &str) -> bool {
        let Ok(expected) = proc_start.parse::<u64>() else {
            return self.is_alive(pid);
        };
        match read_proc_start_ticks(pid) {
            Some(actual) => actual == expected,
            None => self.is_alive(pid),
        }
    }
}

/// Parse the `starttime` field (the 22nd, 1-indexed, per `man 5 proc`) out of
/// the raw contents of a `/proc/<pid>/stat` file.
///
/// Robust against the second field (`comm`, the executable name in
/// parentheses) itself containing spaces or parentheses, by locating the
/// *last* `)` in the line before whitespace-splitting the remaining fields —
/// mirrors the algorithm the `sysinfo` crate uses internally for the same
/// file (it does not expose the raw value publicly; see
/// [`SysinfoProbe::is_alive_matching`]'s doc comment for why that matters
/// here). Field indices below were cross-checked against a real
/// `/proc/<pid>/stat` line on-device, not derived from documentation alone.
fn parse_starttime_ticks(stat: &str) -> Option<u64> {
    let after_comm = stat.rsplit_once(')')?.1;
    // state(0) ppid(1) pgrp(2) session(3) tty(4) tpgid(5) flags(6) minflt(7)
    // cminflt(8) majflt(9) cmajflt(10) utime(11) stime(12) cutime(13)
    // cstime(14) priority(15) nice(16) num_threads(17) itrealvalue(18)
    // starttime(19) — the 20th field after `comm`, i.e. the 22nd overall.
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

/// Read and parse `pid`'s kernel-reported start time, in clock ticks since
/// boot. `None` if there is no `/proc` to read at all, the process doesn't
/// exist, the file can't be read, or its contents don't parse — all treated
/// identically by [`SysinfoProbe::is_alive_matching`]'s caller.
///
/// Only the *read* below is platform-split, and deliberately so: this
/// function stays on every platform's compile path, which is what keeps
/// [`parse_starttime_ticks`] reachable — and therefore compiled and tested —
/// on Windows too. Gating the parse instead makes it dead code there, which
/// is precisely what the Windows CI job caught and what a `clippy` run on
/// Linux structurally cannot see: `-D warnings` can only judge the code its
/// own `cfg`s left standing.
fn read_proc_start_ticks(pid: u32) -> Option<u64> {
    parse_starttime_ticks(&proc_stat(pid)?)
}

/// The raw contents of `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn proc_stat(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()
}

/// No `/proc` on this platform to read a start time from at all — always
/// declines, deferring identity verification to [`SysinfoProbe::is_alive`]'s
/// bare liveness check (see that method's doc comment for why that fallback
/// direction is the safe one).
#[cfg(not(target_os = "linux"))]
fn proc_stat(_pid: u32) -> Option<String> {
    None
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

    #[test]
    fn batched_probe_sees_current_process() {
        let alive = SysinfoProbe::alive_pids([std::process::id(), u32::MAX]);
        assert!(alive.contains(&std::process::id()));
        assert!(!alive.contains(&u32::MAX));
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

        fn is_alive_matching(&self, pid: u32, _proc_start: &str) -> bool {
            self.is_alive(pid)
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

    // --- parse_starttime_ticks --------------------------------------------

    #[test]
    fn parse_starttime_ticks_reads_field_22_after_a_plain_comm() {
        // A real (trimmed) /proc/<pid>/stat line shape: pid (comm) state
        // ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt
        // utime stime cutime cstime priority nice num_threads itrealvalue
        // starttime ...
        let stat = "78739 (claude) S 77993 78739 78739 34818 78739 4194304 \
                     374378 1315564 0 262 8309 4928 1791 1539 20 0 12 0 \
                     1105463 6445989888 108819";
        assert_eq!(parse_starttime_ticks(stat), Some(1_105_463));
    }

    #[test]
    fn parse_starttime_ticks_handles_a_comm_containing_parens_and_spaces() {
        // `comm` (the 2nd field) can itself contain spaces and parentheses;
        // parsing must key off the *last* `)` in the line, not the first.
        let stat = "999 (my (weird) prog) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 \
                     15 16 17 18 4242 0 0";
        assert_eq!(parse_starttime_ticks(stat), Some(4242));
    }

    #[test]
    fn parse_starttime_ticks_is_none_without_a_comm_delimiter() {
        assert_eq!(parse_starttime_ticks("not a stat line at all"), None);
    }

    #[test]
    fn parse_starttime_ticks_is_none_when_too_few_fields_follow_comm() {
        let stat = "1 (sh) S 1 2 3";
        assert_eq!(parse_starttime_ticks(stat), None);
    }

    #[test]
    fn parse_starttime_ticks_is_none_when_the_field_is_not_numeric() {
        let stat = "1 (sh) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 \
                     not-a-number 0 0";
        assert_eq!(parse_starttime_ticks(stat), None);
    }

    // --- SysinfoProbe::is_alive_matching -----------------------------------

    #[test]
    fn is_alive_matching_falls_back_to_is_alive_when_proc_start_does_not_parse() {
        let probe = SysinfoProbe;
        // The current test process is genuinely alive; a garbage proc_start
        // can't be compared against anything, so this must degrade to the
        // same answer plain `is_alive` gives, never to `false`.
        assert!(probe.is_alive_matching(std::process::id(), "not-a-pid-start"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn is_alive_matching_confirms_the_current_process_by_its_real_kernel_start_time() {
        let pid = std::process::id();
        let real_ticks =
            read_proc_start_ticks(pid).expect("this test process must have a readable /proc entry");

        assert!(SysinfoProbe.is_alive_matching(pid, &real_ticks.to_string()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn is_alive_matching_rejects_a_mismatched_start_time_even_though_the_pid_is_alive() {
        // Proves this isn't secretly just `is_alive` again: a live pid with
        // a *wrong* recorded start time (as if the pid had been recycled
        // since the record was written) must read as not-a-match.
        let pid = std::process::id();
        let real_ticks =
            read_proc_start_ticks(pid).expect("this test process must have a readable /proc entry");

        assert!(!SysinfoProbe.is_alive_matching(pid, &(real_ticks + 1).to_string()));
    }
}
