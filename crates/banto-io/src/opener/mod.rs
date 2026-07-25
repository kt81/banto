//! Opening / focusing sessions in a real terminal (psmux, Windows Terminal).
//!
//! Design contract (docs/REQUIREMENTS.md "Opener spec", docs/notes/psmux-spike.md):
//! - Every external process invocation goes through [`CommandRunner`], which
//!   unit tests mock; tests never spawn real processes.
//! - Backend priority: psmux (tmux-compatible CLI) first, Windows Terminal tab
//!   as fallback. Auto detection checks `$TMUX` before `$WT_SESSION`, and
//!   `$WT_SESSION` is only honored when actually running on Windows — under
//!   WSL it can be set (forwarded from the host) without `wt.exe` being a
//!   usable backend at all (see [`detect_backend`]).
//! - psmux pane user options are unusable; panes are tagged with
//!   `select-pane -T` and the store's pane map is the source of truth.
//! - The resume command line and `banto _wrap` are built by the bin crate; this
//!   module receives a ready-made argv + cwd via [`ResumeCommand`].

mod command;
mod tmux;
mod windows_terminal;

pub use command::{CommandOutput, CommandRunner, CommandSpec, SystemCommandRunner};
pub use tmux::{TmuxFlavor, TmuxOpener, TmuxPlacement};
pub use windows_terminal::{WindowsTerminalOpener, WtPlacement};

use std::fmt;
use std::path::PathBuf;

/// Which terminal backend a session is opened into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The `psmux` CLI (Windows-side tmux-compatible implementation).
    Psmux,
    /// Real `tmux`. A separate variant rather than a flavor of
    /// [`Backend::Psmux`] because the two disagree about how a pane is
    /// addressed — see [`TmuxFlavor`], which is where that difference is
    /// spelled out and measured.
    Tmux,
    /// Windows Terminal (`wt`).
    WindowsTerminal,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Backend::Psmux => "psmux",
            Backend::Tmux => "tmux",
            Backend::WindowsTerminal => "Windows Terminal",
        })
    }
}

/// A fully-built resume command plus where to start it.
///
/// The bin crate constructs the `banto _wrap --session <id> -- claude --resume
/// <id>` argv and supplies the session's original cwd; this module only places
/// it into a terminal pane/tab. The opener never builds the resume argv itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCommand {
    /// The session being resumed (used by the caller to key the pane map).
    pub session_id: String,
    /// Full argv to run in the new pane/tab, e.g. `["banto", "_wrap",
    /// "--session", "<id>", "--", "claude", "--resume", "<id>"]`. Passed to the
    /// OS verbatim (no shell), so elements need no quoting.
    pub argv: Vec<String>,
    /// Directory the command starts in (the session's original cwd).
    pub cwd: PathBuf,
    /// Human-visible label used to tag the pane/tab title.
    pub title: String,
}

/// A handle to an opened session, recorded so it can later be focused.
///
/// Returned by [`Opener::open`] and consumed by [`Opener::focus`]. The store
/// persists this alongside the session id as the pane map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionHandle {
    /// A psmux/tmux pane, the window that contains it, and the session that
    /// contains the window. psmux reuses window/pane ids across sessions
    /// (docs/notes/psmux-spike.md), so `session` is required to target the
    /// right one.
    Tmux {
        session: String,
        window_id: String,
        pane_id: String,
    },
    /// A Windows Terminal tab. WT exposes no handle we can target afterwards.
    WindowsTerminal,
}

/// Errors raised while opening or focusing a session.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The backend CLI could not be spawned at all.
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    /// The backend CLI ran but exited unsuccessfully.
    #[error("`{program}` exited with status {code}: {stderr}")]
    Command {
        program: String,
        code: String,
        stderr: String,
    },
    /// The backend CLI succeeded but its output could not be parsed.
    #[error("could not parse `{program}` output: expected {expected}, got {got:?}")]
    UnexpectedOutput {
        program: String,
        expected: String,
        got: String,
    },
    /// Focusing an existing session is not supported by this backend.
    #[error("{backend} cannot focus an existing session")]
    UnsupportedFocus { backend: Backend },
    /// A handle from a different backend was passed to `focus`.
    #[error("handle does not belong to the {backend} backend")]
    MismatchedHandle { backend: Backend },
}

impl OpenError {
    /// Build a [`OpenError::Command`] from a program name and captured status.
    fn command(program: &str, code: Option<i32>, stderr: String) -> Self {
        OpenError::Command {
            program: program.to_string(),
            code: code.map_or_else(|| "signal".to_string(), |c| c.to_string()),
            stderr,
        }
    }
}

/// Opens (resumes) and focuses sessions in a terminal backend.
pub trait Opener {
    /// Which backend this opener drives.
    fn backend(&self) -> Backend;

    /// Resume `cmd` in a new pane/tab and return a handle to it.
    fn open(&self, cmd: &ResumeCommand) -> Result<SessionHandle, OpenError>;

    /// Bring an already-open session (identified by `handle`) to the front.
    fn focus(&self, handle: &SessionHandle) -> Result<(), OpenError>;
}

/// Detect the preferred backend from environment variables and the host
/// platform.
///
/// Order matters: inside psmux both `TMUX` and `WT_SESSION` are set, so `TMUX`
/// is checked first (docs/REQUIREMENTS.md "Opener spec"). `env` looks up a
/// variable by name; `is_windows` is whether this process is actually running
/// on Windows. Both are injected so this stays a pure, testable function
/// (docs/DISCIPLINE.md Appendix A's house pattern) — see
/// [`detect_backend_from_env`] for the real edge values.
///
/// `$WT_SESSION` alone is not enough to select the Windows Terminal backend:
/// under WSL, a shell that forwards it from the Windows Terminal host (e.g.
/// via `WSLENV=WT_SESSION`) sees it set even though the Linux binary itself
/// cannot drive `wt.exe` — the variable is evidence about the host Windows
/// Terminal *ancestry*, not about which platform this process is running on.
/// `Backend::WindowsTerminal` is therefore only ever returned when
/// `is_windows` is also true.
pub fn detect_backend(env: impl Fn(&str) -> Option<String>, is_windows: bool) -> Option<Backend> {
    if env("TMUX").is_some() {
        // Inside a multiplexer — but which CLI drives it? `$TMUX` holds a
        // socket path, not an implementation name, so the platform decides:
        // `psmux` is what banto drives on Windows, real `tmux` everywhere
        // else. Wrong only for a deliberately exotic install (psmux on
        // Linux, tmux under an msys shell on Windows), which is what the
        // explicit `opener = "psmux"` / `"tmux"` config values are for.
        Some(if is_windows {
            Backend::Psmux
        } else {
            Backend::Tmux
        })
    } else if is_windows && env("WT_SESSION").is_some() {
        Some(Backend::WindowsTerminal)
    } else {
        None
    }
}

/// Detect the preferred backend from the current process environment and the
/// real host platform, for a caller that has no injected values of its own.
///
/// Not the only edge that reads them for real: the bin crate's
/// `opener::resolve_backend` keeps its own injected `env` (its callers mock
/// it) and so supplies `cfg!(windows)` itself. Both are edges; the *decision*
/// stays in one place, [`detect_backend`], which is what the tests exercise
/// across every platform × environment combination.
pub fn detect_backend_from_env() -> Option<Backend> {
    detect_backend(|key| std::env::var(key).ok(), cfg!(windows))
}

/// Build the [`Opener`] for `backend`, driven by `runner`.
pub fn opener_for<R: CommandRunner + 'static>(backend: Backend, runner: R) -> Box<dyn Opener> {
    match backend {
        Backend::Psmux => Box::new(TmuxOpener::new(runner, TmuxFlavor::Psmux)),
        Backend::Tmux => Box::new(TmuxOpener::new(runner, TmuxFlavor::Tmux)),
        Backend::WindowsTerminal => Box::new(WindowsTerminalOpener::new(runner)),
    }
}

#[cfg(test)]
mod tests {
    use super::command::mock::MockRunner;
    use super::*;

    #[test]
    fn detect_prefers_tmux_over_windows_terminal_regardless_of_platform() {
        // Inside psmux both variables are set; TMUX must win, on Windows or not.
        let env = |key: &str| match key {
            "TMUX" | "WT_SESSION" => Some("set".to_string()),
            _ => None,
        };
        // The multiplexer wins on both platforms; which CLI drives it is the
        // platform's answer (see `detect_backend`).
        assert_eq!(detect_backend(env, true), Some(Backend::Psmux));
        assert_eq!(detect_backend(env, false), Some(Backend::Tmux));
    }

    #[test]
    fn detect_windows_terminal_when_wt_session_set_and_actually_on_windows() {
        let env = |key: &str| (key == "WT_SESSION").then(|| "set".to_string());
        assert_eq!(detect_backend(env, true), Some(Backend::WindowsTerminal));
    }

    #[test]
    fn wt_session_set_but_not_on_windows_yields_none_the_wsl_case() {
        // WSLENV can forward $WT_SESSION from the Windows Terminal host into
        // a WSL shell even though no tmux/psmux session has been started
        // yet; the Linux binary must not mistake that for "select `wt`".
        let env = |key: &str| (key == "WT_SESSION").then(|| "set".to_string());
        assert_eq!(detect_backend(env, false), None);
    }

    #[test]
    fn detect_none_when_neither_present_regardless_of_platform() {
        assert_eq!(detect_backend(|_| None, true), None);
        assert_eq!(detect_backend(|_| None, false), None);
    }

    #[test]
    fn opener_for_builds_matching_backend() {
        assert_eq!(
            opener_for(Backend::Psmux, MockRunner::new()).backend(),
            Backend::Psmux
        );
        assert_eq!(
            opener_for(Backend::WindowsTerminal, MockRunner::new()).backend(),
            Backend::WindowsTerminal
        );
    }

    #[test]
    fn backend_display_is_human_readable() {
        assert_eq!(Backend::Psmux.to_string(), "psmux");
        assert_eq!(Backend::WindowsTerminal.to_string(), "Windows Terminal");
    }
}
