//! PID liveness behind a mockable trait.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Checks whether a process id refers to a live process.
///
/// Kept behind a trait so unit tests can substitute a mock instead of
/// touching real processes.
pub trait ProcessProbe {
    /// Return `true` if a process with `pid` is currently alive.
    fn is_alive(&self, pid: u32) -> bool;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysinfo_probe_sees_current_process() {
        let probe = SysinfoProbe;
        assert!(probe.is_alive(std::process::id()));
    }
}
