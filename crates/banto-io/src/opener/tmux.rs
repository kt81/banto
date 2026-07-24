//! psmux/tmux-compatible backend for [`Opener`].
//!
//! Command forms follow docs/notes/psmux-spike.md where the spike verified
//! them on a live psmux binary: `new-window`/`split-window` with `-P -F`
//! reliably return freshly created ids, and `select-pane -T` is the only
//! usable way to tag a pane (pane *user options* are set but never read
//! back).
//!
//! The forms the original spike had left unverified were confirmed on-device
//! in a follow-up spike (2026-07-19, see docs/notes/psmux-spike.md): the
//! direct `split-window <command>` spawn form (multi-arg trailing command),
//! the `-c <cwd>` flag, the combined `-F '#{window_id}:#{pane_id}'` format,
//! and that the `psmux` binary itself accepts these subcommands.
//!
//! A later spike (2026-07-20, see docs/notes/psmux-spike.md) found that
//! psmux, unlike real tmux, reuses window/pane ids across sessions — so
//! every target must be session-qualified. It also found that
//! `select-window -t 'session:@window_id'` fails outright (window ids can't
//! be qualified that way), and that `switch-client` corrupted the live
//! server badly enough to destroy a session outright. Both findings shape
//! [`TmuxOpener::focus`]: it never calls `select-window` or `switch-client`,
//! only a session-qualified `select-pane`.

use std::path::Path;

use super::command::{CommandOutput, CommandRunner, CommandSpec};
use super::{Backend, OpenError, Opener, ResumeCommand, SessionHandle};

/// The psmux/tmux CLI binary name.
const PROGRAM: &str = "psmux";

/// Format string for `-P -F`: session, window and pane id, `:`-joined.
/// [`SessionHandle::Tmux`] needs all three (psmux reuses window/pane ids
/// across sessions, docs/notes/psmux-spike.md), and they're all valid in a
/// newly-created pane's format context regardless of which command created it.
const CREATE_FORMAT: &str = "#{session_name}:#{window_id}:#{pane_id}";

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
        let (session, window_id, pane_id) = parse_create_output(&output.stdout)?;
        let target = session_pane_target(&session, &pane_id);

        // Pane user options can't be read back (psmux-spike.md), so the
        // title is the pane's actual `-T`; our store's pane map is the
        // source of truth for the session <-> pane association.
        self.run(CommandSpec::new(
            PROGRAM,
            [
                "select-pane".to_string(),
                "-t".to_string(),
                target,
                "-T".to_string(),
                cmd.title.clone(),
            ],
        ))?;

        Ok(SessionHandle::Tmux {
            session,
            window_id,
            pane_id,
        })
    }

    fn focus(&self, handle: &SessionHandle) -> Result<(), OpenError> {
        let SessionHandle::Tmux {
            session, pane_id, ..
        } = handle
        else {
            return Err(OpenError::MismatchedHandle {
                backend: Backend::Psmux,
            });
        };

        // Session-qualified `select-pane` only (docs/notes/psmux-spike.md,
        // 2026-07-20): `select-window -t 'session:@window_id'` fails
        // outright on psmux, and `switch-client` corrupted the live server
        // badly enough to destroy a session during a spike, so neither is
        // used. banto's own panes are splits within banto's own session
        // (TmuxPlacement::Pane), so a plain session-qualified `select-pane`
        // is sufficient to surface the target pane without switching
        // windows or clients.
        self.run(CommandSpec::new(
            PROGRAM,
            [
                "select-pane".to_string(),
                "-t".to_string(),
                session_pane_target(session, pane_id),
            ],
        ))?;
        Ok(())
    }
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Build a session-qualified pane target (`<session>:<pane_id>`), the
/// reliable form confirmed by docs/notes/psmux-spike.md — no window
/// component is needed for `select-pane`.
fn session_pane_target(session: &str, pane_id: &str) -> String {
    format!("{session}:{pane_id}")
}

/// Parse the `<session>:<window_id>:<pane_id>` line printed by `-P -F` for
/// [`CREATE_FORMAT`].
fn parse_create_output(stdout: &str) -> Result<(String, String, String), OpenError> {
    let line = stdout.trim();
    let mut parts = line.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(session), Some(window_id), Some(pane_id))
            if !session.is_empty() && !window_id.is_empty() && !pane_id.is_empty() =>
        {
            Ok((
                session.to_string(),
                window_id.to_string(),
                pane_id.to_string(),
            ))
        }
        _ => Err(OpenError::UnexpectedOutput {
            program: PROGRAM.to_string(),
            expected: "`<session>:<window_id>:<pane_id>`".to_string(),
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
        let runner = MockRunner::with_responses([CommandOutput::success("play:@3:%8\n")]);
        let opener = TmuxOpener::new(runner);

        let handle = opener.open(&resume_cmd()).unwrap();

        assert_eq!(
            handle,
            SessionHandle::Tmux {
                session: "play".to_string(),
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
                        "#{session_name}:#{window_id}:#{pane_id}",
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
                    ["select-pane", "-t", "play:%8", "-T", "sess-1"].map(str::to_string)
                ),
            ]
        );
    }

    #[test]
    fn open_as_pane_splits_from_the_anchor_when_set() {
        let runner = MockRunner::with_responses([CommandOutput::success("play:@3:%8\n")]);
        let opener = TmuxOpener::new(runner).with_anchor_pane("play:%1");

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
                    "play:%1",
                    "-c",
                    "/home/user/project",
                    "-P",
                    "-F",
                    "#{session_name}:#{window_id}:#{pane_id}",
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
        let runner = MockRunner::with_responses([CommandOutput::success("play:@5:%9")]);
        let opener =
            TmuxOpener::with_placement(runner, TmuxPlacement::Window).with_anchor_pane("play:%1");

        opener.open(&resume_cmd()).unwrap();

        let calls = opener.runner.calls();
        // No `-t` anywhere: new-window creates a whole window, so the anchor
        // (which only disambiguates which pane a split grows from) is moot.
        assert!(!calls[0].args.contains(&"-t".to_string()));
    }

    #[test]
    fn open_as_window_creates_named_window_and_tags() {
        let runner = MockRunner::with_responses([CommandOutput::success("play:@5:%9")]);
        let opener = TmuxOpener::with_placement(runner, TmuxPlacement::Window);

        let handle = opener.open(&resume_cmd()).unwrap();

        assert_eq!(
            handle,
            SessionHandle::Tmux {
                session: "play".to_string(),
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
                    "#{session_name}:#{window_id}:#{pane_id}",
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
                ["select-pane", "-t", "play:%9", "-T", "sess-1"].map(str::to_string)
            )
        );
    }

    #[test]
    fn focus_selects_the_session_qualified_pane_only() {
        let opener = TmuxOpener::new(MockRunner::new());
        let handle = SessionHandle::Tmux {
            session: "play".to_string(),
            window_id: "@3".to_string(),
            pane_id: "%8".to_string(),
        };

        opener.focus(&handle).unwrap();

        // No `select-window`, no `switch-client` (docs/notes/psmux-spike.md,
        // 2026-07-20): the former fails on psmux for `session:@id` targets,
        // the latter corrupted the live server. A session-qualified
        // `select-pane` alone is the only call.
        let calls = opener.runner.calls();
        assert_eq!(
            calls,
            vec![CommandSpec::new(
                "psmux",
                ["select-pane", "-t", "play:%8"].map(str::to_string)
            )]
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

    #[test]
    fn open_errors_on_create_output_missing_the_session_field() {
        // Only two `:`-joined parts (the pre-spike `<window_id>:<pane_id>`
        // format) is malformed now that CREATE_FORMAT requires the session.
        let runner = MockRunner::with_responses([CommandOutput::success("@3:%8\n")]);
        let opener = TmuxOpener::new(runner);

        let err = opener.open(&resume_cmd()).unwrap_err();

        assert!(matches!(err, OpenError::UnexpectedOutput { .. }));
    }
}
