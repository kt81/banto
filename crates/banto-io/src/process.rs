//! Spawning the resumed session's process, behind a mockable trait.
//!
//! `banto-core`'s `CommandRunner` captures the output of short-lived CLI
//! invocations (`psmux`, `wt`). `_wrap`'s child is different: it inherits
//! stdio and runs for the resumed session's entire lifetime, so it gets its
//! own narrow, bin-local trait (CLAUDE.md: every external process invocation
//! sits behind an abstraction that tests can mock).

use std::io;
use std::path::Path;

/// Runs the resumed session's process to completion with inherited stdio.
pub trait ProcessRunner {
    /// Run `argv` (`argv[0]` is the program, the rest its arguments) and
    /// block until it exits. `env_remove` strips these keys from the
    /// inherited environment before the child sees it (see
    /// [`crate::pty::STRIPPED_CHILD_ENV_VARS`] — this is the `_wrap`-side
    /// half of the same fix, since `_wrap` reaches the child on a different
    /// path than the embedded `PtyHost` does). Returns the exit code, or
    /// `None` if the process was terminated by a signal (never observed on
    /// Windows). Inherits this process's own working directory — correct
    /// for `_wrap`, whose own cwd is already the session's cwd (set by the
    /// opener at pane-creation time); in-place mode uses [`Self::run_in`]
    /// instead, since banto's own long-lived process cwd is not the
    /// session's.
    fn run(&self, argv: &[String], env_remove: &[&str]) -> io::Result<Option<i32>>;

    /// Spawn `argv` with inherited stdio (minus `env_remove`, same as
    /// [`Self::run`]) and return a handle for polling it without blocking
    /// (see [`SpawnedProcess`]). Used by the new-session wrap flow, which
    /// needs to keep polling for the session id `claude` creates while the
    /// child is still running — `run`'s blocking `.wait()` can't interleave
    /// with that.
    fn spawn(&self, argv: &[String], env_remove: &[&str]) -> io::Result<Box<dyn SpawnedProcess>>;

    /// Like [`Self::run`], but in `cwd` rather than this process's own
    /// working directory — used by in-place mode (`crate::tui`'s
    /// teardown/run/re-init loop), where banto is a single long-lived
    /// process whose own cwd is not necessarily the session's. `env_remove`
    /// does real work here, not decoration: `run_pending_inplace` passes
    /// [`crate::pty::STRIPPED_CHILD_ENV_VARS`] into this exact parameter,
    /// and the in-place launch route depends on it being honored the same
    /// as [`Self::run`]/[`Self::spawn`]'s. Separately, and only because
    /// this specific path is untested — see [`mock::MockProcessRunner`]'s
    /// own doc for why — no test in this repo would catch that dependency
    /// breaking; only re-measurement would.
    fn run_in(&self, argv: &[String], cwd: &Path, env_remove: &[&str]) -> io::Result<Option<i32>>;
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
    fn run(&self, argv: &[String], env_remove: &[&str]) -> io::Result<Option<i32>> {
        let [program, args @ ..] = argv else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty argv passed to _wrap",
            ));
        };
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        for key in env_remove {
            cmd.env_remove(key);
        }
        let status = cmd.status()?;
        Ok(status.code())
    }

    fn spawn(&self, argv: &[String], env_remove: &[&str]) -> io::Result<Box<dyn SpawnedProcess>> {
        let [program, args @ ..] = argv else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty argv passed to _wrap",
            ));
        };
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        for key in env_remove {
            cmd.env_remove(key);
        }
        let child = cmd.spawn()?;
        Ok(Box::new(SystemSpawnedProcess(child)))
    }

    fn run_in(&self, argv: &[String], cwd: &Path, env_remove: &[&str]) -> io::Result<Option<i32>> {
        let [program, args @ ..] = argv else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty argv passed to run_in",
            ));
        };
        let mut cmd = std::process::Command::new(program);
        cmd.args(args).current_dir(cwd);
        for key in env_remove {
            cmd.env_remove(key);
        }
        let status = cmd.status()?;
        Ok(status.code())
    }
}

/// [`SpawnedProcess`] backed by [`std::process::Child`].
struct SystemSpawnedProcess(std::process::Child);

impl SpawnedProcess for SystemSpawnedProcess {
    fn try_wait(&mut self) -> io::Result<Option<Option<i32>>> {
        Ok(self.0.try_wait()?.map(|status| status.code()))
    }
}

/// Not `#[cfg(test)]`: `wrap.rs` (a different crate, `banto`) needs this for
/// its own tests — see [`crate::pty::mock`]'s doc for why `#[cfg(test)]`
/// can't be used here.
pub mod mock {
    use std::cell::RefCell;
    use std::io;
    use std::path::Path;

    use super::{ProcessRunner, SpawnedProcess};

    /// Records every argv (and, separately, every `env_remove` list) it was
    /// asked to run/spawn, and replies with a canned exit code; never spawns
    /// a real process. `run`/`run_in` report it immediately; a `spawn`ed
    /// [`MockSpawnedProcess`] reports "still running" for
    /// `still_running_polls` polls first (0 by default, i.e. already
    /// exited) — see [`Self::new_spawning`]. `run_in` isn't currently
    /// exercised by any test (in-place mode's `run_pending_inplace` is a
    /// thin, untested-by-design shell, same as `event_loop` — see
    /// `banto::tui`), so unlike `run`/`spawn` it doesn't bother recording
    /// its calls, `env_remove` included.
    #[derive(Debug, Default)]
    pub struct MockProcessRunner {
        exit_code: Option<i32>,
        still_running_polls: usize,
        run_calls: RefCell<Vec<Vec<String>>>,
        spawn_calls: RefCell<Vec<Vec<String>>>,
        run_env_removes: RefCell<Vec<Vec<String>>>,
        spawn_env_removes: RefCell<Vec<Vec<String>>>,
    }

    impl MockProcessRunner {
        pub fn new(exit_code: Option<i32>) -> Self {
            Self {
                exit_code,
                ..Self::default()
            }
        }

        /// A runner whose `spawn`ed process reports "still running" for
        /// `still_running_polls` `try_wait` calls before exiting with
        /// `exit_code`.
        pub fn new_spawning(still_running_polls: usize, exit_code: Option<i32>) -> Self {
            Self {
                exit_code,
                still_running_polls,
                ..Self::default()
            }
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.run_calls.borrow().clone()
        }

        pub fn spawn_calls(&self) -> Vec<Vec<String>> {
            self.spawn_calls.borrow().clone()
        }

        /// Every `env_remove` list passed to [`ProcessRunner::run`], in call
        /// order.
        pub fn run_env_removes(&self) -> Vec<Vec<String>> {
            self.run_env_removes.borrow().clone()
        }

        /// Every `env_remove` list passed to [`ProcessRunner::spawn`], in
        /// call order.
        pub fn spawn_env_removes(&self) -> Vec<Vec<String>> {
            self.spawn_env_removes.borrow().clone()
        }
    }

    impl ProcessRunner for MockProcessRunner {
        fn run(&self, argv: &[String], env_remove: &[&str]) -> io::Result<Option<i32>> {
            self.run_calls.borrow_mut().push(argv.to_vec());
            self.run_env_removes
                .borrow_mut()
                .push(env_remove.iter().map(|s| s.to_string()).collect());
            Ok(self.exit_code)
        }

        fn spawn(
            &self,
            argv: &[String],
            env_remove: &[&str],
        ) -> io::Result<Box<dyn SpawnedProcess>> {
            self.spawn_calls.borrow_mut().push(argv.to_vec());
            self.spawn_env_removes
                .borrow_mut()
                .push(env_remove.iter().map(|s| s.to_string()).collect());
            Ok(Box::new(MockSpawnedProcess::new(
                self.still_running_polls,
                self.exit_code,
            )))
        }

        fn run_in(
            &self,
            _argv: &[String],
            _cwd: &Path,
            _env_remove: &[&str],
        ) -> io::Result<Option<i32>> {
            Ok(self.exit_code)
        }
    }

    /// A [`SpawnedProcess`] that reports "still running" for the first
    /// `still_running_polls` calls to `try_wait`, then exits with
    /// `exit_code` on every call after that.
    pub struct MockSpawnedProcess {
        remaining: usize,
        exit_code: Option<i32>,
    }

    impl MockSpawnedProcess {
        pub fn new(still_running_polls: usize, exit_code: Option<i32>) -> Self {
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
        let err = SystemProcessRunner.run(&[], &[]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn run_in_empty_argv_is_rejected() {
        let err = SystemProcessRunner
            .run_in(&[], Path::new("."), &[])
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
