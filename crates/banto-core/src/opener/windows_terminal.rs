//! Windows Terminal backend for [`Opener`].
//!
//! `wt` has no API to enumerate or focus existing tabs/windows
//! (docs/REQUIREMENTS.md "Opener spec"), so [`SessionHandle::WindowsTerminal`]
//! carries no data to focus with, and [`WindowsTerminalOpener::focus`] always
//! fails with [`OpenError::UnsupportedFocus`]. A future HWND-based "one
//! session = one window" mode is out of scope here.

use super::command::{CommandRunner, CommandSpec};
use super::{Backend, OpenError, Opener, ResumeCommand, SessionHandle};

/// The Windows Terminal CLI binary name.
const PROGRAM: &str = "wt";

/// Where a resumed session is placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WtPlacement {
    /// Add a tab to the nearest existing `wt` window (`-w 0`).
    Tab,
    /// Force a brand new `wt` window (`-w -1`).
    Window,
}

impl WtPlacement {
    fn window_arg(self) -> &'static str {
        match self {
            WtPlacement::Tab => "0",
            WtPlacement::Window => "-1",
        }
    }
}

/// [`Opener`] backed by the `wt` CLI, driven through [`CommandRunner`].
#[derive(Debug)]
pub struct WindowsTerminalOpener<R> {
    runner: R,
    placement: WtPlacement,
}

impl<R> WindowsTerminalOpener<R> {
    /// A [`WtPlacement::Tab`] opener (default: reuse the existing `wt` window).
    pub fn new(runner: R) -> Self {
        Self::with_placement(runner, WtPlacement::Tab)
    }

    /// An opener using an explicit placement.
    pub fn with_placement(runner: R, placement: WtPlacement) -> Self {
        Self { runner, placement }
    }
}

impl<R: CommandRunner> Opener for WindowsTerminalOpener<R> {
    fn backend(&self) -> Backend {
        Backend::WindowsTerminal
    }

    fn open(&self, cmd: &ResumeCommand) -> Result<SessionHandle, OpenError> {
        let mut args = vec![
            "-w".to_string(),
            self.placement.window_arg().to_string(),
            "new-tab".to_string(),
            "-d".to_string(),
            cmd.cwd.to_string_lossy().into_owned(),
            "--title".to_string(),
            cmd.title.clone(),
            "--".to_string(),
        ];
        args.extend(cmd.argv.iter().cloned());

        let output = self.runner.run(&CommandSpec::new(PROGRAM, args))?;
        if !output.success {
            return Err(OpenError::command(PROGRAM, output.code, output.stderr));
        }
        Ok(SessionHandle::WindowsTerminal)
    }

    fn focus(&self, handle: &SessionHandle) -> Result<(), OpenError> {
        match handle {
            SessionHandle::WindowsTerminal => Err(OpenError::UnsupportedFocus {
                backend: Backend::WindowsTerminal,
            }),
            SessionHandle::Tmux { .. } => Err(OpenError::MismatchedHandle {
                backend: Backend::WindowsTerminal,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::command::CommandOutput;
    use super::super::command::mock::MockRunner;
    use super::*;

    fn resume_cmd() -> ResumeCommand {
        ResumeCommand {
            session_id: "sess-1".to_string(),
            argv: [
                "banto",
                "_wrap",
                "--session",
                "sess-1",
                "--",
                "claude",
                "--resume",
                "sess-1",
            ]
            .map(str::to_string)
            .to_vec(),
            cwd: PathBuf::from("/home/user/project"),
            title: "sess-1".to_string(),
        }
    }

    #[test]
    fn backend_is_windows_terminal() {
        assert_eq!(
            WindowsTerminalOpener::new(MockRunner::new()).backend(),
            Backend::WindowsTerminal
        );
    }

    #[test]
    fn open_creates_tab_in_nearest_window_by_default() {
        let runner = MockRunner::new();
        let opener = WindowsTerminalOpener::new(runner);

        let handle = opener.open(&resume_cmd()).unwrap();

        assert_eq!(handle, SessionHandle::WindowsTerminal);
        let calls = opener.runner.calls();
        assert_eq!(
            calls,
            vec![CommandSpec::new(
                "wt",
                [
                    "-w",
                    "0",
                    "new-tab",
                    "-d",
                    "/home/user/project",
                    "--title",
                    "sess-1",
                    "--",
                    "banto",
                    "_wrap",
                    "--session",
                    "sess-1",
                    "--",
                    "claude",
                    "--resume",
                    "sess-1",
                ]
                .map(str::to_string)
            )]
        );
    }

    #[test]
    fn open_forces_new_window_when_placement_is_window() {
        let runner = MockRunner::new();
        let opener = WindowsTerminalOpener::with_placement(runner, WtPlacement::Window);

        opener.open(&resume_cmd()).unwrap();

        let calls = opener.runner.calls();
        assert_eq!(
            calls,
            vec![CommandSpec::new(
                "wt",
                [
                    "-w",
                    "-1",
                    "new-tab",
                    "-d",
                    "/home/user/project",
                    "--title",
                    "sess-1",
                    "--",
                    "banto",
                    "_wrap",
                    "--session",
                    "sess-1",
                    "--",
                    "claude",
                    "--resume",
                    "sess-1",
                ]
                .map(str::to_string)
            )]
        );
    }

    #[test]
    fn open_errors_when_wt_exits_nonzero() {
        let runner = MockRunner::with_responses([CommandOutput::failure(
            Some(1),
            "wt: unknown option".to_string(),
        )]);
        let opener = WindowsTerminalOpener::new(runner);

        let err = opener.open(&resume_cmd()).unwrap_err();

        assert!(matches!(err, OpenError::Command { .. }));
    }

    #[test]
    fn focus_is_unsupported() {
        let opener = WindowsTerminalOpener::new(MockRunner::new());

        let err = opener.focus(&SessionHandle::WindowsTerminal).unwrap_err();

        assert!(matches!(
            err,
            OpenError::UnsupportedFocus {
                backend: Backend::WindowsTerminal
            }
        ));
    }

    #[test]
    fn focus_rejects_tmux_handle() {
        let opener = WindowsTerminalOpener::new(MockRunner::new());
        let handle = SessionHandle::Tmux {
            session: "0".to_string(),
            window_id: "@1".to_string(),
            pane_id: "%1".to_string(),
        };

        let err = opener.focus(&handle).unwrap_err();

        assert!(matches!(
            err,
            OpenError::MismatchedHandle {
                backend: Backend::WindowsTerminal
            }
        ));
    }
}
