//! Spawning the resumed session's process, behind a mockable trait.
//!
//! `banto-core`'s `CommandRunner` captures the output of short-lived CLI
//! invocations (`psmux`, `wt`). `_wrap`'s child is different: it inherits
//! stdio and runs for the resumed session's entire lifetime, so it gets its
//! own narrow, bin-local trait (CLAUDE.md: every external process invocation
//! sits behind an abstraction that tests can mock).

use std::io;

/// Runs the resumed session's process to completion with inherited stdio.
pub trait ProcessRunner {
    /// Run `argv` (`argv[0]` is the program, the rest its arguments) and
    /// block until it exits. Returns the exit code, or `None` if the process
    /// was terminated by a signal (never observed on Windows).
    fn run(&self, argv: &[String]) -> io::Result<Option<i32>>;

    /// Spawn `argv` with inherited stdio and return a handle for polling it
    /// without blocking (see [`SpawnedProcess`]). Used by the new-session
    /// wrap flow, which needs to keep polling for the session id `claude`
    /// creates while the child is still running — `run`'s blocking `.wait()`
    /// can't interleave with that.
    fn spawn(&self, argv: &[String]) -> io::Result<Box<dyn SpawnedProcess>>;
}

/// A spawned, possibly still-running child process — see
/// [`ProcessRunner::spawn`].
pub trait SpawnedProcess {
    /// Poll without blocking: `Ok(None)` while still running, `Ok(Some(code))`
    /// once it has exited (`code` is `None` if terminated by a signal).
    fn try_wait(&mut self) -> io::Result<Option<Option<i32>>>;
}

/// [`ProcessRunner`] backed by [`std::process::Command`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn run(&self, argv: &[String]) -> io::Result<Option<i32>> {
        let [program, args @ ..] = argv else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty argv passed to _wrap",
            ));
        };
        let status = std::process::Command::new(program).args(args).status()?;
        Ok(status.code())
    }

    fn spawn(&self, argv: &[String]) -> io::Result<Box<dyn SpawnedProcess>> {
        let [program, args @ ..] = argv else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty argv passed to _wrap",
            ));
        };
        let child = std::process::Command::new(program).args(args).spawn()?;
        Ok(Box::new(SystemSpawnedProcess(child)))
    }
}

/// [`SpawnedProcess`] backed by [`std::process::Child`].
struct SystemSpawnedProcess(std::process::Child);

impl SpawnedProcess for SystemSpawnedProcess {
    fn try_wait(&mut self) -> io::Result<Option<Option<i32>>> {
        Ok(self.0.try_wait()?.map(|status| status.code()))
    }
}

#[cfg(test)]
pub(crate) mod mock {
    use std::cell::RefCell;
    use std::io;

    use super::{ProcessRunner, SpawnedProcess};

    /// Records every argv it was asked to run/spawn and replies with a
    /// canned exit code; never spawns a real process. `run` reports it
    /// immediately; a `spawn`ed [`MockSpawnedProcess`] reports "still
    /// running" for `still_running_polls` polls first (0 by default, i.e.
    /// already exited) — see [`Self::new_spawning`].
    #[derive(Debug, Default)]
    pub(crate) struct MockProcessRunner {
        exit_code: Option<i32>,
        still_running_polls: usize,
        run_calls: RefCell<Vec<Vec<String>>>,
        spawn_calls: RefCell<Vec<Vec<String>>>,
    }

    impl MockProcessRunner {
        pub(crate) fn new(exit_code: Option<i32>) -> Self {
            Self {
                exit_code,
                ..Self::default()
            }
        }

        /// A runner whose `spawn`ed process reports "still running" for
        /// `still_running_polls` `try_wait` calls before exiting with
        /// `exit_code`.
        pub(crate) fn new_spawning(still_running_polls: usize, exit_code: Option<i32>) -> Self {
            Self {
                exit_code,
                still_running_polls,
                ..Self::default()
            }
        }

        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            self.run_calls.borrow().clone()
        }

        pub(crate) fn spawn_calls(&self) -> Vec<Vec<String>> {
            self.spawn_calls.borrow().clone()
        }
    }

    impl ProcessRunner for MockProcessRunner {
        fn run(&self, argv: &[String]) -> io::Result<Option<i32>> {
            self.run_calls.borrow_mut().push(argv.to_vec());
            Ok(self.exit_code)
        }

        fn spawn(&self, argv: &[String]) -> io::Result<Box<dyn SpawnedProcess>> {
            self.spawn_calls.borrow_mut().push(argv.to_vec());
            Ok(Box::new(MockSpawnedProcess::new(
                self.still_running_polls,
                self.exit_code,
            )))
        }
    }

    /// A [`SpawnedProcess`] that reports "still running" for the first
    /// `still_running_polls` calls to `try_wait`, then exits with
    /// `exit_code` on every call after that.
    pub(crate) struct MockSpawnedProcess {
        remaining: usize,
        exit_code: Option<i32>,
    }

    impl MockSpawnedProcess {
        pub(crate) fn new(still_running_polls: usize, exit_code: Option<i32>) -> Self {
            Self {
                remaining: still_running_polls,
                exit_code,
            }
        }
    }

    impl SpawnedProcess for MockSpawnedProcess {
        fn try_wait(&mut self) -> io::Result<Option<Option<i32>>> {
            if self.remaining == 0 {
                Ok(Some(self.exit_code))
            } else {
                self.remaining -= 1;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_argv_is_rejected() {
        let err = SystemProcessRunner.run(&[]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
