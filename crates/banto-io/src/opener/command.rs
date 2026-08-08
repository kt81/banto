//! External command execution behind a mockable trait (AGENTS.md invariant:
//! external invocations sit behind an abstraction) — see [`CommandRunner`],
//! and `opener`'s own module doc for the design contract this serves.

use super::OpenError;

/// A single external command: a program plus its arguments.
///
/// Arguments are handed to the OS verbatim (no shell), so values may contain
/// spaces or other special characters without any quoting concerns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    /// Build a spec from a program name and its arguments.
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
        }
    }
}

/// The captured result of running a [`CommandSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Whether the process exited successfully (exit code 0).
    pub success: bool,
    /// Exit code if the process exited normally; `None` if killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    /// A successful (exit 0) result carrying `stdout` and empty stderr.
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            success: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A failed result with the given exit `code` and `stderr`.
    pub fn failure(code: Option<i32>, stderr: impl Into<String>) -> Self {
        Self {
            success: false,
            code,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

/// Runs external commands.
pub trait CommandRunner {
    /// Run `spec` to completion, capturing stdout/stderr.
    ///
    /// Returns `Ok` whenever the process executed, regardless of its exit
    /// status; `Err` only when the process could not be spawned. Callers
    /// inspect [`CommandOutput::success`] to decide whether the run was
    /// acceptable.
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, OpenError>;
}

/// [`CommandRunner`] backed by [`std::process::Command`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, OpenError> {
        let output = std::process::Command::new(&spec.program)
            .args(&spec.args)
            .output()
            .map_err(|source| OpenError::Spawn {
                program: spec.program.clone(),
                source,
            })?;
        Ok(CommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
pub(crate) mod mock {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::super::OpenError;
    use super::{CommandOutput, CommandRunner, CommandSpec};

    /// A [`CommandRunner`] that records every command and replays queued
    /// outputs in order (falling back to an empty success once exhausted).
    #[derive(Debug, Default)]
    pub(crate) struct MockRunner {
        responses: RefCell<VecDeque<CommandOutput>>,
        calls: RefCell<Vec<CommandSpec>>,
    }

    impl MockRunner {
        /// A runner whose calls all succeed with empty stdout.
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// A runner returning `responses` in order, then empty successes.
        pub(crate) fn with_responses(responses: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                calls: RefCell::default(),
            }
        }

        /// The specs passed to [`CommandRunner::run`], in call order.
        pub(crate) fn calls(&self) -> Vec<CommandSpec> {
            self.calls.borrow().clone()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, OpenError> {
            self.calls.borrow_mut().push(spec.clone());
            Ok(self
                .responses
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| CommandOutput::success("")))
        }
    }
}
