//! psmux/tmux-compatible backend for [`Opener`].
//!
//! Command forms follow docs/notes/psmux-spike.md where the spike verified
//! them on a live psmux binary: `new-window`/`split-window` with `-P -F`
//! reliably return freshly created ids, `select-pane -T` is the only usable
//! way to tag a pane (pane *user options* are set but never read back), and
//! focusing an existing session needs an explicit `select-window` +
//! `select-pane` pair.
//!
//! The forms the original spike had left unverified were confirmed on-device
//! in a follow-up spike (2026-07-19, see docs/notes/psmux-spike.md): the
//! direct `split-window <command>` spawn form (multi-arg trailing command),
//! the `-c <cwd>` flag, the combined `-F '#{window_id}:#{pane_id}'` format,
//! and that the `psmux` binary itself accepts these subcommands.

use std::path::Path;

use super::command::{CommandOutput, CommandRunner, CommandSpec};
use super::{Backend, OpenError, Opener, ResumeCommand, SessionHandle};

/// The psmux/tmux CLI binary name.
const PROGRAM: &str = "psmux";

/// Format string for `-P -F`: both ids, `:`-joined. `new-window` and
/// `split-window` only expose one of `window_id`/`pane_id` directly, but
/// [`SessionHandle::Tmux`] needs both, and they're both valid in a
/// newly-created pane's format context regardless of which command created it.
const CREATE_FORMAT: &str = "#{window_id}:#{pane_id}";

/// Where a resumed session is placed relative to the window banto runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxPlacement {
    /// Split the current window (`split-window -h`).
    Pane,
    /// Create a new window (`new-window`).
    Window,
}

/// [`Opener`] backed by a psmux/tmux-compatible CLI, driven through [`CommandRunner`].
#[derive(Debug)]
pub struct TmuxOpener<R> {
    runner: R,
    placement: TmuxPlacement,
    anchor_pane: Option<String>,
}

impl<R> TmuxOpener<R> {
    /// A [`TmuxPlacement::Pane`] opener: sessions live as panes of a single
    /// banto-managed window rather than spawning a new window each time.
    pub fn new(runner: R) -> Self {
        Self::with_placement(runner, TmuxPlacement::Pane)
    }

    /// An opener using an explicit placement.
    pub fn with_placement(runner: R, placement: TmuxPlacement) -> Self {
        Self {
            runner,
            placement,
            anchor_pane: None,
        }
    }

    /// Anchor pane splits are created from (only affects [`TmuxPlacement::Pane`]).
    ///
    /// `split-window` with no `-t` targets the *client's* currently active
    /// pane, not banto's own — confirmed on-device: a resume pane can sprout
    /// in whatever window the user happens to have focused instead of next
    /// to banto. Setting this anchors every split on a fixed pane (the
    /// caller resolves it from `$TMUX_PANE`) regardless of where the user's
    /// tmux client is currently looking. `new-window` is unaffected: it
    /// creates a whole window rather than splitting an existing pane, so it
    /// has no equivalent ambiguity.
    pub fn with_anchor_pane(mut self, anchor_pane: impl Into<String>) -> Self {
        self.anchor_pane = Some(anchor_pane.into());
        self
    }
}

impl<R: CommandRunner> TmuxOpener<R> {
    /// Run `spec`, turning a non-zero exit into [`OpenError::Command`].
    fn run(&self, spec: CommandSpec) -> Result<CommandOutput, OpenError> {
        let output = self.runner.run(&spec)?;
        if output.success {
            Ok(output)
        } else {
            Err(OpenError::command(
                &spec.program,
                output.code,
                output.stderr,
            ))
        }
    }
}

impl<R: CommandRunner> Opener for TmuxOpener<R> {
    fn backend(&self) -> Backend {
        Backend::Psmux
    }

    fn open(&self, cmd: &ResumeCommand) -> Result<SessionHandle, OpenError> {
        let cwd = path_arg(&cmd.cwd);
        let mut args = match self.placement {
            TmuxPlacement::Window => vec![
                "new-window".to_string(),
                "-d".to_string(),
                "-n".to_string(),
                cmd.title.clone(),
                "-c".to_string(),
                cwd,
                "-P".to_string(),
                "-F".to_string(),
                CREATE_FORMAT.to_string(),
            ],
            TmuxPlacement::Pane => {
                let mut args = vec!["split-window".to_string(), "-h".to_string()];
                if let Some(anchor) = &self.anchor_pane {
                    args.push("-t".to_string());
                    args.push(anchor.clone());
                }
                args.extend([
                    "-c".to_string(),
                    cwd,
                    "-P".to_string(),
                    "-F".to_string(),
                    CREATE_FORMAT.to_string(),
                ]);
                args
            }
        };
        args.extend(cmd.argv.iter().cloned());

        let output = self.run(CommandSpec::new(PROGRAM, args))?;
        let (window_id, pane_id) = parse_create_output(&output.stdout)?;

        // Pane user options can't be read back (psmux-spike.md), so the
        // title is the pane's actual `-T`; our store's pane map is the
        // source of truth for the session <-> pane association.
        self.run(CommandSpec::new(
            PROGRAM,
            [
                "select-pane".to_string(),
                "-t".to_string(),
                pane_id.clone(),
                "-T".to_string(),
                cmd.title.clone(),
            ],
        ))?;

        Ok(SessionHandle::Tmux { window_id, pane_id })
    }

    fn focus(&self, handle: &SessionHandle) -> Result<(), OpenError> {
        let SessionHandle::Tmux { window_id, pane_id } = handle else {
            return Err(OpenError::MismatchedHandle {
                backend: Backend::Psmux,
            });
        };

        self.run(CommandSpec::new(
            PROGRAM,
            [
                "select-window".to_string(),
                "-t".to_string(),
                window_id.clone(),
            ],
        ))?;
        self.run(CommandSpec::new(
            PROGRAM,
            ["select-pane".to_string(), "-t".to_string(), pane_id.clone()],
        ))?;
        Ok(())
    }
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Parse the `<window_id>:<pane_id>` line printed by `-P -F` for [`CREATE_FORMAT`].
fn parse_create_output(stdout: &str) -> Result<(String, String), OpenError> {
    let line = stdout.trim();
    match line.split_once(':') {
        Some((window_id, pane_id)) if !window_id.is_empty() && !pane_id.is_empty() => {
            Ok((window_id.to_string(), pane_id.to_string()))
        }
        _ => Err(OpenError::UnexpectedOutput {
            program: PROGRAM.to_string(),
            expected: "`<window_id>:<pane_id>`".to_string(),
            got: stdout.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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
    fn backend_is_psmux() {
        assert_eq!(TmuxOpener::new(MockRunner::new()).backend(), Backend::Psmux);
    }

    #[test]
    fn open_as_pane_splits_current_window_and_tags() {
        let runner = MockRunner::with_responses([CommandOutput::success("@3:%8\n")]);
        let opener = TmuxOpener::new(runner);

        let handle = opener.open(&resume_cmd()).unwrap();

        assert_eq!(
            handle,
            SessionHandle::Tmux {
                window_id: "@3".to_string(),
                pane_id: "%8".to_string(),
            }
        );
        let calls = opener.runner.calls();
        assert_eq!(
            calls,
            vec![
                CommandSpec::new(
                    "psmux",
                    [
                        "split-window",
                        "-h",
                        "-c",
                        "/home/user/project",
                        "-P",
                        "-F",
                        "#{window_id}:#{pane_id}",
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
                ),
                CommandSpec::new(
                    "psmux",
                    ["select-pane", "-t", "%8", "-T", "sess-1"].map(str::to_string)
                ),
            ]
        );
    }

    #[test]
    fn open_as_pane_splits_from_the_anchor_when_set() {
        let runner = MockRunner::with_responses([CommandOutput::success("@3:%8\n")]);
        let opener = TmuxOpener::new(runner).with_anchor_pane("%1");

        opener.open(&resume_cmd()).unwrap();

        let calls = opener.runner.calls();
        assert_eq!(
            calls[0],
            CommandSpec::new(
                "psmux",
                [
                    "split-window",
                    "-h",
                    "-t",
                    "%1",
                    "-c",
                    "/home/user/project",
                    "-P",
                    "-F",
                    "#{window_id}:#{pane_id}",
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
            )
        );
    }

    #[test]
    fn anchor_pane_does_not_affect_window_placement() {
        let runner = MockRunner::with_responses([CommandOutput::success("@5:%9")]);
        let opener =
            TmuxOpener::with_placement(runner, TmuxPlacement::Window).with_anchor_pane("%1");

        opener.open(&resume_cmd()).unwrap();

        let calls = opener.runner.calls();
        // No `-t` anywhere: new-window creates a whole window, so the anchor
        // (which only disambiguates which pane a split grows from) is moot.
        assert!(!calls[0].args.contains(&"-t".to_string()));
    }

    #[test]
    fn open_as_window_creates_named_window_and_tags() {
        let runner = MockRunner::with_responses([CommandOutput::success("@5:%9")]);
        let opener = TmuxOpener::with_placement(runner, TmuxPlacement::Window);

        let handle = opener.open(&resume_cmd()).unwrap();

        assert_eq!(
            handle,
            SessionHandle::Tmux {
                window_id: "@5".to_string(),
                pane_id: "%9".to_string(),
            }
        );
        let calls = opener.runner.calls();
        assert_eq!(
            calls[0],
            CommandSpec::new(
                "psmux",
                [
                    "new-window",
                    "-d",
                    "-n",
                    "sess-1",
                    "-c",
                    "/home/user/project",
                    "-P",
                    "-F",
                    "#{window_id}:#{pane_id}",
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
            )
        );
        assert_eq!(
            calls[1],
            CommandSpec::new(
                "psmux",
                ["select-pane", "-t", "%9", "-T", "sess-1"].map(str::to_string)
            )
        );
    }

    #[test]
    fn focus_selects_window_then_pane() {
        let opener = TmuxOpener::new(MockRunner::new());
        let handle = SessionHandle::Tmux {
            window_id: "@3".to_string(),
            pane_id: "%8".to_string(),
        };

        opener.focus(&handle).unwrap();

        let calls = opener.runner.calls();
        assert_eq!(
            calls,
            vec![
                CommandSpec::new("psmux", ["select-window", "-t", "@3"].map(str::to_string)),
                CommandSpec::new("psmux", ["select-pane", "-t", "%8"].map(str::to_string)),
            ]
        );
    }

    #[test]
    fn focus_rejects_windows_terminal_handle() {
        let opener = TmuxOpener::new(MockRunner::new());
        let err = opener.focus(&SessionHandle::WindowsTerminal).unwrap_err();
        assert!(matches!(
            err,
            OpenError::MismatchedHandle {
                backend: Backend::Psmux
            }
        ));
    }

    #[test]
    fn open_errors_when_create_command_fails() {
        let runner = MockRunner::with_responses([CommandOutput::failure(
            Some(1),
            "no server running".to_string(),
        )]);
        let opener = TmuxOpener::new(runner);

        let err = opener.open(&resume_cmd()).unwrap_err();

        assert!(matches!(err, OpenError::Command { .. }));
    }

    #[test]
    fn open_errors_on_unparseable_create_output() {
        let runner = MockRunner::with_responses([CommandOutput::success("not-an-id\n")]);
        let opener = TmuxOpener::new(runner);

        let err = opener.open(&resume_cmd()).unwrap_err();

        assert!(matches!(err, OpenError::UnexpectedOutput { .. }));
    }
}
