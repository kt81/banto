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
}

#[cfg(test)]
pub(crate) mod mock {
    use std::cell::RefCell;
    use std::io;

    use super::ProcessRunner;

    /// Records every argv it was asked to run and replies with a canned exit
    /// code; never spawns a real process.
    #[derive(Debug, Default)]
    pub(crate) struct MockProcessRunner {
        exit_code: Option<i32>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl MockProcessRunner {
        pub(crate) fn new(exit_code: Option<i32>) -> Self {
            Self {
                exit_code,
                calls: RefCell::default(),
            }
        }

        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }
    }

    impl ProcessRunner for MockProcessRunner {
        fn run(&self, argv: &[String]) -> io::Result<Option<i32>> {
            self.calls.borrow_mut().push(argv.to_vec());
            Ok(self.exit_code)
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
