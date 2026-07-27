//! Bin-level wiring between `banto-io`'s `Opener`/`Store` and the resume
//! flow triggered from the TUI.
//!
//! Decides whether a session's existing pane is still alive (focus it) or a
//! new one must be spawned (open it and record where it landed), enforcing
//! the "never resume a session twice" invariant (CLAUDE.md invariant 4;
//! docs/REQUIREMENTS.md "Opener spec"). Liveness is judged purely by PID (the
//! `banto _wrap` process registers its own PID once it starts — see
//! [`crate::wrap`] — mirroring how `banto_io::status` judges session
//! activity), never by querying the terminal backend itself.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use banto_core::config::{AgentBinaries, OpenerMode};
pub use banto_core::model::SessionToOpen;
use banto_core::model::{AgentKind, SessionId};
use banto_io::opener::{
    self, Backend, CommandRunner, CommandSpec, OpenError, Opener, ResumeCommand, SessionHandle,
    TmuxFlavor, TmuxOpener, WindowsTerminalOpener,
};
use banto_io::status::{LiveSession, ProcessProbe};
use banto_io::store::{PaneRecord, Store, StoreError};

/// Outcome of an open/focus attempt, for the caller to turn into a status message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOutcome {
    Focused,
    Opened,
    /// A pane exists and is (presumed) alive, but this backend cannot focus
    /// an existing pane (Windows Terminal). Refuses to open a second one.
    AlreadyOpenCannotFocus,
    /// No pane record exists for this session (so there's nothing to focus),
    /// but its own live-state file shows it's still running — most likely a
    /// session launched via the `n` new-session modal, which has no
    /// pre-existing session id to key a pane record against. Refuses to
    /// `--resume` it, since that would fork its history (CLAUDE.md
    /// invariant 4) while it's already open somewhere banto can't locate.
    AlreadyRunningUntracked,
    /// No backend could be determined (`OpenerMode::Auto` with neither
    /// `$TMUX` nor `$WT_SESSION` set).
    NoBackendDetected,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionOpenError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Open(#[from] OpenError),
}

/// Resolve the backend for a *split* placement (the `s` key) — never
/// consulted for in-place activation (Enter), which bypasses backend
/// resolution entirely. `Auto` detects it from the environment via `env`
/// (injected so this stays deterministic in tests); production code passes
/// `std::env::var`. `InPlace` (the config default) has no split backend of
/// its own, so `s` falls back to the same auto-detection as `Auto` — it's
/// only the *default action* (Enter) that in-place mode changes, not
/// whether a split is still available on request.
///
/// The real host platform (`cfg!(windows)`) is supplied here, at this
/// `is_windows` is injected rather than read here, for the same reason `env`
/// is: `Auto`/`InPlace` now resolve `$TMUX` to a *different* backend
/// depending on the platform (real `tmux`, or `psmux` on Windows), so a
/// `cfg!(windows)` read inside this function would make its own behavior —
/// and every test of it — silently host-dependent. Passed through, as its
/// function's own edge, to [`opener::detect_backend`] — which takes it as an
/// injected parameter rather than reading it itself, so its own platform
/// gating (e.g. never selecting Windows Terminal under WSL just because
/// `$WT_SESSION` leaked in) stays unit-tested on every CI platform. This
/// function's signature intentionally does not grow a matching parameter:
/// both of its callers (`crate::tui`) always want the real platform, so
/// there is nothing for them to inject, and `detect_backend`'s own tests
/// already cover every platform/env combination this delegates to.
pub fn resolve_backend(
    mode: OpenerMode,
    env: impl Fn(&str) -> Option<String>,
    is_windows: bool,
) -> Option<Backend> {
    match mode {
        OpenerMode::InPlace | OpenerMode::Auto => opener::detect_backend(env, is_windows),
        OpenerMode::Psmux => Some(Backend::Psmux),
        OpenerMode::Tmux => Some(Backend::Tmux),
        OpenerMode::WindowsTerminal => Some(Backend::WindowsTerminal),
    }
}

/// The tmux-compatible flavor `backend` drives, or `None` for a backend that
/// is not one at all. Keeps the two `Opener` construction sites from having
/// to enumerate every multiplexer variant separately — adding a flavor is a
/// change here and in `banto_io::opener`, not at every call site.
fn tmux_flavor(backend: Backend) -> Option<TmuxFlavor> {
    match backend {
        Backend::Psmux => Some(TmuxFlavor::Psmux),
        Backend::Tmux => Some(TmuxFlavor::Tmux),
        Backend::WindowsTerminal => None,
    }
}

/// Launch a brand-new (non-resumed) `claude` session in `cwd`, via `banto
/// _wrap --new-session` so it can later be found and focused. This is the
/// split-placement counterpart to in-place's `opener::inplace_argv(None)` —
/// used by the new-session modal's `N` (split) placement, alongside `n`
/// (in-place, the default) — see `crate::app::NewSessionPlacement`.
///
/// Unlike [`open_session`], there is no pre-existing session id to key a
/// double-resume check or a pane-map record against here in the opener —
/// starting two new sessions is not a double resume, it's just two new
/// sessions — so this skips the store entirely. But the session Claude
/// creates for `cwd` *is* discovered and recorded, just not by this
/// function: `_wrap --new-session` (`crate::wrap::run_new_session`) does
/// that itself once it's running inside the new pane, since only it can
/// observe both where it landed and which session id eventually shows up.
///
/// `wrap_log`, when given, is the caller's own resolved `BANTO_WRAP_LOG`
/// value, passed through to `_wrap` via `--wrap-log` (see
/// [`new_session_wrap_argv`]) rather than left for it to read from its own
/// environment, which a psmux-spawned process does not reliably inherit.
pub fn open_new_session<R: CommandRunner + 'static>(
    backend: Option<Backend>,
    cwd: &Path,
    runner: R,
    anchor_pane: Option<&str>,
    wrap_log: Option<&str>,
    binaries: &AgentBinaries,
) -> Result<OpenOutcome, OpenError> {
    let Some(backend) = backend else {
        return Ok(OpenOutcome::NoBackendDetected);
    };
    let title = cwd
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.display().to_string());
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string));
    let cmd = ResumeCommand {
        // Unused: there is no pane-map record for this launch to key.
        session_id: String::new(),
        argv: new_session_wrap_argv(exe.as_deref(), cwd, backend, wrap_log, binaries),
        cwd: cwd.to_path_buf(),
        title,
    };
    let opener: Box<dyn Opener> = match (tmux_flavor(backend), anchor_pane) {
        (Some(flavor), Some(anchor)) => {
            Box::new(TmuxOpener::new(runner, flavor).with_anchor_pane(anchor))
        }
        (Some(flavor), None) => Box::new(TmuxOpener::new(runner, flavor)),
        (None, _) => Box::new(WindowsTerminalOpener::new(runner)),
    };
    opener.open(&cmd)?;
    Ok(OpenOutcome::Opened)
}

/// Read-only inputs an open/resume decision needs beyond the session itself
/// — bundled once `open_session`'s own argument list hit clippy's
/// too-many-arguments limit, rather than letting it keep growing one field
/// at a time.
pub struct OpenContext<'a> {
    pub probe: &'a dyn ProcessProbe,
    /// The current `<claude_home>/sessions/*.json` snapshot (see
    /// `banto_io::status::read_live_sessions`).
    pub live: &'a [LiveSession],
    pub binaries: &'a AgentBinaries,
}

/// Open or focus `session`, enforcing the no-double-resume invariant.
///
/// `backend` is the resolved opener backend for *new* opens (see
/// [`resolve_backend`]); an existing pane is always focused through whichever
/// backend it was originally opened with, regardless of the current default.
/// `ctx.live` is used only for the untracked case below.
pub fn open_session<R: CommandRunner + Clone + 'static>(
    store: &Store,
    backend: Option<Backend>,
    session: &SessionToOpen,
    runner: R,
    anchor_pane: Option<&str>,
    ctx: &OpenContext,
) -> Result<OpenOutcome, SessionOpenError> {
    let id = SessionId(session.id.clone());

    match store.get_pane(&id)? {
        Some(record) => {
            // No PID yet means the wrapper is still starting up; assume alive
            // rather than risk a double resume.
            let alive = record.pid.is_none_or(|pid| ctx.probe.is_alive(pid));
            if alive {
                match focus_existing(store, &record, runner.clone())? {
                    FocusResult::Outcome(outcome) => return Ok(outcome),
                    // The backend CLI ran but reported the pane doesn't
                    // exist any more (already cleaned up); fall through to
                    // open fresh.
                    FocusResult::Stale => {}
                }
            } else {
                // The wrapper is gone but never cleaned up (e.g. it crashed).
                store.remove_pane(&id)?;
            }
        }
        // No pane record — but the session's own live-state file shows it's
        // still running (e.g. it was launched via the `n` new-session
        // modal, which has no pre-existing session id to key a pane record
        // against). Opening fresh here would `--resume` an already-running
        // session, forking its history.
        None if is_live(&session.id, ctx.live, ctx.probe) => {
            return Ok(OpenOutcome::AlreadyRunningUntracked);
        }
        None => {}
    }

    open_fresh(store, backend, session, runner, anchor_pane, ctx.binaries)
}

/// Whether any live-state entry names `session_id` with a currently-alive
/// pid, regardless of busy/idle status — used to guard against
/// `--resume`-ing (and thereby history-forking) a session that's live but
/// untracked (see [`open_session`]), and reused by [`decide_inplace_resume`]
/// for the same reason: in-place mode has no pane map of its own, so this is
/// its *only* double-resume guard, not just a fallback for the untracked
/// case. Unlike `banto_io::status::classify` (which is about which
/// activity dot to show, so a busy entry outranks a merely-alive one), this
/// only answers "is this session actually running right now".
///
/// When `entry` carries a `proc_start` (Claude Code's own record of its
/// process's kernel start time), liveness is checked via
/// [`ProcessProbe::is_alive_matching`] instead of a bare
/// [`ProcessProbe::is_alive`] — so a pid recycled by an unrelated process
/// since the live-state file was written is not mistaken for the original
/// session still running, which would otherwise block a resume forever with
/// no self-heal (see that method's doc comment).
pub(crate) fn is_live(session_id: &str, live: &[LiveSession], probe: &dyn ProcessProbe) -> bool {
    live.iter().any(|entry| {
        entry.session_id.as_deref() == Some(session_id)
            && match &entry.proc_start {
                Some(proc_start) => probe.is_alive_matching(entry.pid, proc_start),
                None => probe.is_alive(entry.pid),
            }
    })
}

/// A terminal hand-off ready to run in-place: the argv to execute with
/// inherited stdio, the directory to run it in, and a one-line status
/// `crate::tui::run_pending_inplace` prints (a plain, non-destructive
/// `println!` — no screen clear, no cursor repositioning) right before
/// spawning it (see [`resume_startup_message`]/[`new_session_startup_message`])
/// — resuming can take several seconds, and without this the user just sees
/// a bare, silent shell in that gap.
///
/// A `Clear(ClearType::All)` + centered-block approach was tried and
/// reverted: it wiped the terminal's existing scrollback (destructive to
/// the user's own history, not just banto's), and if `claude` exited
/// quickly without painting anything of its own, the centered message was
/// left behind on an otherwise-blank screen even after banto itself had
/// exited. A single appended line degrades far more gracefully — it just
/// sits in the scrollback like any other command's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InPlaceLaunch {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub startup_message: String,
}

/// The startup message for resuming `title` in place (see [`InPlaceLaunch`]).
/// `title` is expected to already be the display title-or-id-fallback (see
/// `SessionToOpen::title`, built from `SessionRow::display_title`) — plain
/// ASCII wording so it's safe at any terminal width; `title` itself is
/// user/session data and is printed as-is.
pub(crate) fn resume_startup_message(title: &str) -> String {
    format!("banto: resuming \"{title}\" ... (returns to the list when the session closes)")
}

/// The startup message for launching a new session in place at `cwd` (see
/// [`InPlaceLaunch`]).
pub(crate) fn new_session_startup_message(cwd: &Path) -> String {
    format!("banto: starting a new session in {} ...", cwd.display())
}

/// A pending agent invocation, independent of *how* it will be run —
/// in-place, wrapped via `_wrap --session`, or wrapped via `_wrap
/// --new-session` (see [`inplace_argv`]/[`wrap_argv`]/[`new_session_wrap_argv`],
/// the sites that build one of these and turn it into argv via [`Self::argv`]).
/// This is the single place in the workspace that knows which flags each
/// agent product takes and in what order.
///
/// One variant per [`AgentKind`], not one struct with optional fields
/// spanning every product: `append_system_prompt`/`mcp_config` are Claude
/// Code concepts with no Codex equivalent (Codex 0.145's own `--help` has
/// no `--append-system-prompt`; its MCP config goes through
/// `-c mcp_servers.…` instead), and brigades — the only callers of either
/// field — are Claude-only for now. A struct with both fields left `None`
/// for Codex would rely on every caller remembering to leave them unset; an
/// enum makes a Codex launch physically unable to carry them, whether or
/// not a caller remembers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentLaunch {
    Claude {
        resume: Option<String>,
        model: Option<String>,
        append_system_prompt: Option<String>,
        mcp_config: Option<PathBuf>,
    },
    /// Codex takes its working directory as `-C <dir>` in argv itself
    /// (measured against Codex 0.145: unlike Claude, which only ever gets a
    /// cwd via the spawner's own `current_dir`/`Command::current_dir`) — so
    /// `cwd` lives on the launch here, even though [`Self::Claude`]'s own
    /// rendering keeps ignoring it in favor of the spawner.
    Codex {
        resume: Option<String>,
        model: Option<String>,
        cwd: PathBuf,
    },
}

/// Resolve the binary to invoke for `agent`: the operator's own
/// `[agent_binaries]` override from config.toml, or the product's own bare
/// name (found via `$PATH`) when unset — today's behavior, preserved as the
/// default for both products.
pub(crate) fn agent_binary(agent: AgentKind, binaries: &AgentBinaries) -> String {
    let (override_path, default_name) = match agent {
        AgentKind::ClaudeCode => (&binaries.claude, "claude"),
        AgentKind::Codex => (&binaries.codex, "codex"),
    };
    match override_path {
        Some(path) => path.to_string_lossy().into_owned(),
        None => default_name.to_string(),
    }
}

impl AgentLaunch {
    /// Render as `binary`'s own argv (see [`agent_binary`] for resolving
    /// it). Claude's fixed order is the one every existing call site already
    /// agreed on before this type existed: `--resume`, then `--model`, then
    /// `--append-system-prompt`, then `--mcp-config`. Codex's: the `resume
    /// <uuid>` subcommand (or nothing, for a fresh launch), then `-m`, then
    /// `-C <cwd>` last, always present.
    pub(crate) fn argv(&self, binary: &str) -> Vec<String> {
        let mut argv = vec![binary.to_string()];
        match self {
            AgentLaunch::Claude {
                resume,
                model,
                append_system_prompt,
                mcp_config,
            } => {
                if let Some(id) = resume {
                    argv.push("--resume".to_string());
                    argv.push(id.clone());
                }
                if let Some(model) = model {
                    argv.push("--model".to_string());
                    argv.push(model.clone());
                }
                if let Some(prompt) = append_system_prompt {
                    argv.push("--append-system-prompt".to_string());
                    argv.push(prompt.clone());
                }
                if let Some(path) = mcp_config {
                    argv.push("--mcp-config".to_string());
                    argv.push(path.to_string_lossy().into_owned());
                }
            }
            AgentLaunch::Codex { resume, model, cwd } => {
                if let Some(id) = resume {
                    argv.push("resume".to_string());
                    argv.push(id.clone());
                }
                if let Some(model) = model {
                    argv.push("-m".to_string());
                    argv.push(model.clone());
                }
                argv.push("-C".to_string());
                argv.push(cwd.to_string_lossy().into_owned());
            }
        }
        argv
    }
}

/// Build the in-place launch argv. Unlike [`wrap_argv`]/[`new_session_wrap_argv`]
/// (the split-placement equivalents), there is no `banto _wrap` wrapper:
/// in-place mode blocks on the child directly as banto's own terminal (see
/// `crate::tui`'s teardown/run/re-init loop), so there is no separate
/// long-lived process for a wrapper to attach a pane record to in the first
/// place.
pub(crate) fn inplace_argv(
    agent: AgentKind,
    session_id: Option<&str>,
    cwd: &Path,
    binaries: &AgentBinaries,
) -> Vec<String> {
    let launch = match agent {
        AgentKind::ClaudeCode => AgentLaunch::Claude {
            resume: session_id.map(str::to_string),
            model: None,
            append_system_prompt: None,
            mcp_config: None,
        },
        AgentKind::Codex => AgentLaunch::Codex {
            resume: session_id.map(str::to_string),
            model: None,
            cwd: cwd.to_path_buf(),
        },
    };
    launch.argv(&agent_binary(agent, binaries))
}

/// Decide whether to resume `session` in place, or `None` to refuse: taking
/// over the terminal for a session that's already running somewhere else (a
/// split pane, another banto instance, or a directly-launched `claude`)
/// would `--resume` it a second time and fork its history (CLAUDE.md
/// invariant 4). `live`/`probe` are the same live-state snapshot and
/// liveness probe [`open_session`]'s untracked-but-live guard uses (see
/// [`is_live`]) — the only guard available here, since in-place mode has no
/// pane map of its own to consult first.
pub(crate) fn decide_inplace_resume(
    session: &SessionToOpen,
    probe: &dyn ProcessProbe,
    live: &[LiveSession],
    binaries: &AgentBinaries,
) -> Option<InPlaceLaunch> {
    if is_live(&session.id, live, probe) {
        None
    } else {
        Some(InPlaceLaunch {
            argv: inplace_argv(session.agent, Some(&session.id), &session.cwd, binaries),
            cwd: session.cwd.clone(),
            startup_message: resume_startup_message(&session.title),
        })
    }
}

/// Outcome of [`focus_existing`]: either a final [`OpenOutcome`], or a
/// signal that the pane record was stale and the caller should open fresh.
enum FocusResult {
    Outcome(OpenOutcome),
    Stale,
}

fn focus_existing<R: CommandRunner + 'static>(
    store: &Store,
    record: &PaneRecord,
    runner: R,
) -> Result<FocusResult, SessionOpenError> {
    let Some(backend) = parse_backend_key(&record.backend) else {
        // Unknown backend string (e.g. written by a future banto version).
        store.remove_pane(&record.session_id)?;
        return Ok(FocusResult::Outcome(OpenOutcome::NoBackendDetected));
    };
    let Some(handle) = decode_handle(backend, &record.target) else {
        store.remove_pane(&record.session_id)?;
        return Ok(FocusResult::Outcome(OpenOutcome::NoBackendDetected));
    };

    match opener::opener_for(backend, runner).focus(&handle) {
        Ok(()) => Ok(FocusResult::Outcome(OpenOutcome::Focused)),
        Err(OpenError::UnsupportedFocus { .. }) => {
            Ok(FocusResult::Outcome(OpenOutcome::AlreadyOpenCannotFocus))
        }
        Err(OpenError::Command { .. }) => {
            // The backend CLI ran but failed (e.g. psmux's `select-window`
            // reporting no such window): the pane is actually gone even
            // though our record didn't reflect it yet. Stale, not a real
            // error — clean up and let the caller open a fresh one.
            store.remove_pane(&record.session_id)?;
            Ok(FocusResult::Stale)
        }
        Err(err) => Err(err.into()),
    }
}

fn open_fresh<R: CommandRunner + 'static>(
    store: &Store,
    backend: Option<Backend>,
    session: &SessionToOpen,
    runner: R,
    anchor_pane: Option<&str>,
    binaries: &AgentBinaries,
) -> Result<OpenOutcome, SessionOpenError> {
    let Some(backend) = backend else {
        return Ok(OpenOutcome::NoBackendDetected);
    };

    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string));
    let cmd = ResumeCommand {
        session_id: session.id.clone(),
        argv: wrap_argv(
            exe.as_deref(),
            &session.id,
            session.agent,
            &session.cwd,
            binaries,
        ),
        cwd: session.cwd.clone(),
        title: session.title.clone(),
    };
    // Anchor psmux splits on banto's own pane (resolved from TMUX_PANE by
    // the caller) so the resume pane lands next to banto instead of in
    // whatever window the user's client is currently focused on.
    let opener: Box<dyn Opener> = match (tmux_flavor(backend), anchor_pane) {
        (Some(flavor), Some(anchor)) => {
            Box::new(TmuxOpener::new(runner, flavor).with_anchor_pane(anchor))
        }
        (Some(flavor), None) => Box::new(TmuxOpener::new(runner, flavor)),
        (None, _) => Box::new(WindowsTerminalOpener::new(runner)),
    };
    let handle = opener.open(&cmd)?;

    store.set_pane(&PaneRecord {
        session_id: SessionId(session.id.clone()),
        backend: backend_key(backend).to_string(),
        target: encode_target(&handle),
        // Not yet known: `banto _wrap` registers its own PID once it starts
        // (see `crate::wrap::register_pid`).
        pid: None,
        opened_at: SystemTime::now(),
    })?;

    Ok(OpenOutcome::Opened)
}

/// Build the `<banto> _wrap --session <id> -- claude --resume <id>` argv.
///
/// `exe` is the resolved path to the running `banto` binary (production
/// callers pass `std::env::current_exe()`, injected here so this stays
/// deterministic in tests) so the spawned pane/tab can find it even when it
/// isn't on `$PATH` (e.g. a dev build); `None` falls back to the bare name
/// `banto` (relies on `$PATH`).
fn wrap_argv(
    exe: Option<&str>,
    session_id: &str,
    agent: AgentKind,
    cwd: &Path,
    binaries: &AgentBinaries,
) -> Vec<String> {
    let banto_exe = exe.unwrap_or("banto").to_string();
    let mut argv = vec![
        banto_exe,
        "_wrap".to_string(),
        "--session".to_string(),
        session_id.to_string(),
        "--".to_string(),
    ];
    let launch = match agent {
        AgentKind::ClaudeCode => AgentLaunch::Claude {
            resume: Some(session_id.to_string()),
            model: None,
            append_system_prompt: None,
            mcp_config: None,
        },
        AgentKind::Codex => AgentLaunch::Codex {
            resume: Some(session_id.to_string()),
            model: None,
            cwd: cwd.to_path_buf(),
        },
    };
    argv.extend(launch.argv(&agent_binary(agent, binaries)));
    argv
}

/// Build the `<banto> _wrap --new-session --cwd <cwd> --backend <key>
/// [--wrap-log <path>] -- claude` argv (see [`wrap_argv`] for the
/// resume-mode equivalent and the `exe` convention). Unlike the resume
/// case, `_wrap` itself discovers the session id Claude assigns and writes
/// its own pane record (`crate::wrap::run_new_session`) — there's no id
/// known yet for this process to be told. `backend` is passed through
/// explicitly (this function's own already-resolved choice) rather than
/// left for `_wrap` to re-detect from its environment — see
/// `crate::wrap::resolve_own_pane`'s doc comment for why that would be
/// unreliable.
///
/// `wrap_log`, when set, is banto's own `BANTO_WRAP_LOG` value, passed
/// through explicitly for the same kind of reason: a psmux-spawned `_wrap`
/// process does not reliably inherit banto's own environment
/// (docs/notes/psmux-spike.md), so relying on `_wrap` to read
/// `BANTO_WRAP_LOG` from its own environment would silently produce no
/// diagnostic log at all — see `crate::wrap::WrapLog::new`'s doc comment.
///
/// Always launches Claude: the new-session modal this feeds has no agent
/// axis of its own, and the chōba (`crate::tui`) is feature-frozen — adding
/// one is out of scope, not an oversight. `binaries` still resolves which
/// `claude` binary, for the same configurability every other launch site
/// gets.
fn new_session_wrap_argv(
    exe: Option<&str>,
    cwd: &Path,
    backend: Backend,
    wrap_log: Option<&str>,
    binaries: &AgentBinaries,
) -> Vec<String> {
    let banto_exe = exe.unwrap_or("banto").to_string();
    let mut argv = vec![
        banto_exe,
        "_wrap".to_string(),
        "--new-session".to_string(),
        "--cwd".to_string(),
        cwd.display().to_string(),
        "--backend".to_string(),
        backend_key(backend).to_string(),
    ];
    if let Some(path) = wrap_log {
        argv.push("--wrap-log".to_string());
        argv.push(path.to_string());
    }
    argv.push("--".to_string());
    let launch = AgentLaunch::Claude {
        resume: None,
        model: None,
        append_system_prompt: None,
        mcp_config: None,
    };
    argv.extend(launch.argv(&agent_binary(AgentKind::ClaudeCode, binaries)));
    argv
}

/// Stable string stored in [`PaneRecord::backend`] (kebab-case, independent
/// of `Backend`'s human-readable `Display`). `pub(crate)`: also used by
/// `crate::wrap`'s new-session flow, which writes its own pane record
/// directly (there's no pre-existing one from an `open_*` call for it to
/// attach to).
pub(crate) fn backend_key(backend: Backend) -> &'static str {
    match backend {
        Backend::Psmux => "psmux",
        Backend::Tmux => "tmux",
        Backend::WindowsTerminal => "windows-terminal",
    }
}

/// Inverse of [`backend_key`]. `pub(crate)`: also used by `main`'s `_wrap
/// --new-session --backend <key>` parsing.
pub(crate) fn parse_backend_key(key: &str) -> Option<Backend> {
    match key {
        "psmux" => Some(Backend::Psmux),
        "tmux" => Some(Backend::Tmux),
        "windows-terminal" => Some(Backend::WindowsTerminal),
        _ => None,
    }
}

/// Encode a [`SessionHandle`] into [`PaneRecord::target`]. `pub(crate)`: see
/// [`backend_key`].
pub(crate) fn encode_target(handle: &SessionHandle) -> String {
    match handle {
        SessionHandle::Tmux {
            session,
            window_id,
            pane_id,
        } => format!("{session}:{window_id}:{pane_id}"),
        SessionHandle::WindowsTerminal => String::new(),
    }
}

/// Inverse of [`encode_target`].
///
/// A pane record written before pane/window ids were session-qualified (see
/// [`resolve_own_anchor`]'s doc) encodes the old, un-qualified
/// `<window_id>:<pane_id>` form (two `:`-joined parts) and decodes to `None`
/// here — the caller ([`focus_existing`]) treats a `None` decode as a stale
/// record and opens a fresh pane instead of risking an ambiguous focus.
fn decode_handle(backend: Backend, target: &str) -> Option<SessionHandle> {
    match backend {
        // Both multiplexer flavors store the same three parts: the encoded
        // target is banto's own record of where a pane is, not a CLI target
        // string, so it does not vary with how that CLI wants panes
        // addressed (see `banto_io::opener::TmuxFlavor`).
        Backend::Psmux | Backend::Tmux => {
            let mut parts = target.splitn(3, ':');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(session), Some(window_id), Some(pane_id))
                    if !session.is_empty() && !window_id.is_empty() && !pane_id.is_empty() =>
                {
                    Some(SessionHandle::Tmux {
                        session: session.to_string(),
                        window_id: window_id.to_string(),
                        pane_id: pane_id.to_string(),
                    })
                }
                _ => None,
            }
        }
        Backend::WindowsTerminal => Some(SessionHandle::WindowsTerminal),
    }
}

/// Resolve banto's own session-qualified anchor pane (`"<session>:<pane>"`)
/// for a psmux split, so a resume/new-session pane always lands next to
/// banto's own pane unambiguously — psmux reuses pane/window ids across
/// sessions (docs/notes/psmux-spike.md), so a bare pane id is no longer
/// enough to anchor a split correctly.
///
/// `backend` is the caller's own already-resolved choice, trusted rather
/// than guessed from `tmux_pane`'s presence (mirrors
/// `crate::wrap::resolve_own_pane`'s reasoning: banto can be running inside
/// an unrelated psmux session while still configured to open new sessions
/// via Windows Terminal). `tmux_pane` is banto's own bare `$TMUX_PANE` (e.g.
/// `%2`); `None` (not running inside psmux at all) always yields `None`.
///
/// If session resolution itself fails (the `display-message` call errors,
/// or banto somehow isn't attached to a psmux session despite `$TMUX_PANE`
/// being set), falls back to the bare pane id — best-effort, matching the
/// pre-session-qualification behavior — rather than refusing to anchor the
/// split at all.
pub(crate) fn resolve_own_anchor<R: CommandRunner>(
    backend: Option<Backend>,
    runner: &R,
    tmux_pane: Option<&str>,
) -> Option<String> {
    if backend != Some(Backend::Psmux) {
        return None;
    }
    let pane = tmux_pane?;
    match resolve_own_session(runner) {
        Some(session) => Some(format!("{session}:{pane}")),
        None => Some(pane.to_string()),
    }
}

/// Resolve banto's own psmux session name via `display-message -p
/// '#{session_name}'` (no `-t`, so it targets banto's own pane — the
/// reliable route per docs/notes/psmux-spike.md; the `$TMUX` env var's
/// session-id field is explicitly documented there as unreliable).
fn resolve_own_session<R: CommandRunner>(runner: &R) -> Option<String> {
    let spec = CommandSpec::new(
        "psmux",
        ["display-message", "-p", "#{session_name}"].map(str::to_string),
    );
    let output = runner.run(&spec).ok()?;
    if !output.success {
        return None;
    }
    let session = output.stdout.trim();
    (!session.is_empty()).then(|| session.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::model::AgentKind;
    use banto_io::opener::{CommandOutput, CommandSpec};
    use std::cell::{Cell, RefCell};
    use std::collections::{HashSet, VecDeque};
    use std::rc::Rc;

    /// Records every command and always reports success with a fixed
    /// session-qualified `create` output for commands that need to parse
    /// one (psmux's `-P -F` create format, now `<session>:<window>:<pane>`
    /// — docs/notes/psmux-spike.md, 2026-07-20) — unless configured via
    /// [`MockRunner::failing_select_pane`] to simulate the backend CLI
    /// reporting the pane no longer exists. Cheaply `Clone`-able (shared
    /// call log) so a clone can be moved into `open_session` while the
    /// original is kept around to inspect the calls afterward.
    #[derive(Clone, Default)]
    struct MockRunner {
        calls: Rc<RefCell<Vec<CommandSpec>>>,
        fail_select_pane: Rc<Cell<bool>>,
        /// Queued responses consumed before falling back to the default
        /// create-format success (see [`MockRunner::with_target_responses`]).
        responses: Rc<RefCell<VecDeque<CommandOutput>>>,
    }

    impl MockRunner {
        fn calls(&self) -> Vec<CommandSpec> {
            self.calls.borrow().clone()
        }

        /// A runner whose focus-time `select-pane` call fails, simulating
        /// psmux reporting the pane is gone (e.g. it was closed out of
        /// band). `focus` (`TmuxOpener::focus`) only ever calls
        /// `select-pane -t <target>` now — never
        /// `select-window`/`switch-client` (docs/notes/psmux-spike.md,
        /// 2026-07-20) — so that 3-arg form (no `-T`) is the only call this
        /// fails; `open`'s own tagging `select-pane -t <target> -T <title>`
        /// call (used when this runner also serves a fallback fresh-open)
        /// is left alone, distinguished by the presence of `-T`.
        fn failing_select_pane() -> Self {
            let runner = Self::default();
            runner.fail_select_pane.set(true);
            runner
        }

        /// A runner returning `responses` in order for its first calls (used
        /// to test [`resolve_own_anchor`] in isolation from the psmux
        /// create-format response below), then falling back to the default
        /// behavior once exhausted.
        fn with_target_responses(responses: impl IntoIterator<Item = CommandOutput>) -> Self {
            Self {
                responses: Rc::new(RefCell::new(responses.into_iter().collect())),
                ..Self::default()
            }
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, OpenError> {
            self.calls.borrow_mut().push(spec.clone());
            if let Some(response) = self.responses.borrow_mut().pop_front() {
                return Ok(response);
            }
            let is_focus_select_pane = spec.args.first().map(String::as_str) == Some("select-pane")
                && !spec.args.iter().any(|a| a == "-T");
            if self.fail_select_pane.get() && is_focus_select_pane {
                return Ok(CommandOutput::failure(Some(1), "no such pane".to_string()));
            }
            Ok(CommandOutput::success("play:@1:%1\n"))
        }
    }

    struct MockProbe {
        alive: HashSet<u32>,
        /// Pids that are alive (per `is_alive`) but whose `is_alive_matching`
        /// must still report `false` regardless of the given `proc_start` —
        /// simulates a pid recycled by an unrelated process since a
        /// live-state file's `proc_start` was recorded.
        identity_mismatch: HashSet<u32>,
    }

    impl MockProbe {
        fn with_alive(pids: &[u32]) -> Self {
            Self {
                alive: pids.iter().copied().collect(),
                identity_mismatch: HashSet::new(),
            }
        }

        /// A probe whose given pids are alive but never match on identity —
        /// see [`Self::identity_mismatch`].
        fn with_alive_but_identity_mismatch(pids: &[u32]) -> Self {
            Self {
                alive: pids.iter().copied().collect(),
                identity_mismatch: pids.iter().copied().collect(),
            }
        }
    }

    impl ProcessProbe for MockProbe {
        fn is_alive(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }

        fn parent_pid(&self, _pid: u32) -> Option<u32> {
            None
        }

        fn is_alive_matching(&self, pid: u32, _proc_start: &str) -> bool {
            self.is_alive(pid) && !self.identity_mismatch.contains(&pid)
        }
    }

    fn session() -> SessionToOpen {
        SessionToOpen {
            id: "sess-1".to_string(),
            agent: AgentKind::ClaudeCode,
            title: "Fix login".to_string(),
            cwd: PathBuf::from("/work/alpha"),
        }
    }

    #[test]
    fn opens_fresh_and_records_the_pane_when_nothing_is_tracked() {
        let store = Store::open_in_memory().unwrap();
        let probe = MockProbe::with_alive(&[]);
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &OpenContext {
                probe: &probe,
                live: &[],
                binaries: &AgentBinaries::default(),
            },
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let record = store
            .get_pane(&SessionId("sess-1".to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(record.backend, "psmux");
        assert_eq!(record.target, "play:@1:%1");
        assert_eq!(record.pid, None);
    }

    /// A synthetic live-state entry, e.g. a session launched via the `n`
    /// new-session modal (no pane record) that's still running.
    fn live_entry(session_id: &str, pid: u32) -> LiveSession {
        LiveSession {
            pid,
            session_id: Some(session_id.to_string()),
            cwd: None,
            status: None,
            kind: None,
            name: None,
            proc_start: None,
        }
    }

    /// Same as [`live_entry`], but carrying a `proc_start` — exercises
    /// `is_live`'s [`ProcessProbe::is_alive_matching`] path instead of the
    /// bare [`ProcessProbe::is_alive`] one.
    fn live_entry_with_proc_start(session_id: &str, pid: u32, proc_start: &str) -> LiveSession {
        LiveSession {
            proc_start: Some(proc_start.to_string()),
            ..live_entry(session_id, pid)
        }
    }

    #[test]
    fn is_live_matches_only_a_live_entry_with_the_right_session_id_and_a_live_pid() {
        let probe = MockProbe::with_alive(&[100]);

        assert!(is_live("sess-1", &[live_entry("sess-1", 100)], &probe));
        // Wrong session id.
        assert!(!is_live("sess-1", &[live_entry("sess-2", 100)], &probe));
        // Right session id, dead pid.
        assert!(!is_live("sess-1", &[live_entry("sess-1", 999)], &probe));
        // No live entries at all.
        assert!(!is_live("sess-1", &[], &probe));
    }

    #[test]
    fn is_live_confirms_a_proc_start_carrying_entry_via_is_alive_matching() {
        let probe = MockProbe::with_alive(&[100]);
        let entry = live_entry_with_proc_start("sess-1", 100, "1105463");

        assert!(is_live("sess-1", &[entry], &probe));
    }

    #[test]
    fn is_live_rejects_a_recycled_pid_even_though_it_is_alive() {
        // The pid is alive per a bare liveness check, but the probe reports
        // it belongs to a different process than the one that recorded
        // `proc_start` (e.g. the original session crashed and the pid was
        // reused) — is_live must not treat this as still running.
        let probe = MockProbe::with_alive_but_identity_mismatch(&[100]);
        let entry = live_entry_with_proc_start("sess-1", 100, "1105463");

        assert!(!is_live("sess-1", &[entry], &probe));
    }

    // --- AgentLaunch::argv --------------------------------------------------

    fn blank_claude() -> AgentLaunch {
        AgentLaunch::Claude {
            resume: None,
            model: None,
            append_system_prompt: None,
            mcp_config: None,
        }
    }

    #[test]
    fn agent_launch_claude_with_no_fields_is_bare_claude() {
        assert_eq!(
            blank_claude().argv("claude"),
            ["claude"].map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_appends_mcp_config_last_on_its_own() {
        let launch = AgentLaunch::Claude {
            resume: None,
            model: None,
            append_system_prompt: None,
            mcp_config: Some(PathBuf::from("C:/data/banto/mcp/1-worker-1.json")),
        };
        assert_eq!(
            launch.argv("claude"),
            [
                "claude",
                "--mcp-config",
                "C:/data/banto/mcp/1-worker-1.json"
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_combines_every_flag_in_the_fixed_order() {
        let launch = AgentLaunch::Claude {
            resume: Some("sess-1".to_string()),
            model: Some("opus".to_string()),
            append_system_prompt: Some("you are the Director".to_string()),
            mcp_config: Some(PathBuf::from("C:/data/banto/mcp/1-director.json")),
        };
        assert_eq!(
            launch.argv("claude"),
            [
                "claude",
                "--resume",
                "sess-1",
                "--model",
                "opus",
                "--append-system-prompt",
                "you are the Director",
                "--mcp-config",
                "C:/data/banto/mcp/1-director.json",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_codex_fresh_launch_is_bare_codex_plus_cwd() {
        let launch = AgentLaunch::Codex {
            resume: None,
            model: None,
            cwd: PathBuf::from("/work/alpha"),
        };
        assert_eq!(
            launch.argv("codex"),
            ["codex", "-C", "/work/alpha"].map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_codex_resume_renders_the_subcommand_then_model_then_cwd() {
        let launch = AgentLaunch::Codex {
            resume: Some("codex-uuid-1".to_string()),
            model: Some("o3".to_string()),
            cwd: PathBuf::from("/work/alpha"),
        };
        assert_eq!(
            launch.argv("codex"),
            [
                "codex",
                "resume",
                "codex-uuid-1",
                "-m",
                "o3",
                "-C",
                "/work/alpha",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn agent_binary_falls_back_to_the_bare_name_when_unset() {
        let binaries = AgentBinaries::default();
        assert_eq!(agent_binary(AgentKind::ClaudeCode, &binaries), "claude");
        assert_eq!(agent_binary(AgentKind::Codex, &binaries), "codex");
    }

    #[test]
    fn agent_binary_uses_the_configured_override_when_set() {
        let binaries = AgentBinaries {
            claude: None,
            codex: Some(PathBuf::from("C:/tools/codex.exe")),
        };
        assert_eq!(agent_binary(AgentKind::ClaudeCode, &binaries), "claude");
        assert_eq!(
            agent_binary(AgentKind::Codex, &binaries),
            "C:/tools/codex.exe"
        );
    }

    // --- inplace_argv / decide_inplace_resume -----------------------------

    #[test]
    fn inplace_argv_resumes_a_given_session_id() {
        assert_eq!(
            inplace_argv(
                AgentKind::ClaudeCode,
                Some("sess-1"),
                Path::new("/work/alpha"),
                &AgentBinaries::default(),
            ),
            ["claude", "--resume", "sess-1"].map(str::to_string)
        );
    }

    #[test]
    fn inplace_argv_launches_plain_claude_for_a_new_session() {
        assert_eq!(
            inplace_argv(
                AgentKind::ClaudeCode,
                None,
                Path::new("/work/alpha"),
                &AgentBinaries::default(),
            ),
            ["claude"].map(str::to_string)
        );
    }

    #[test]
    fn inplace_argv_codex_resume_includes_the_cwd_flag() {
        assert_eq!(
            inplace_argv(
                AgentKind::Codex,
                Some("codex-uuid-1"),
                Path::new("/work/alpha"),
                &AgentBinaries::default(),
            ),
            ["codex", "resume", "codex-uuid-1", "-C", "/work/alpha"].map(str::to_string)
        );
    }

    #[test]
    fn resume_startup_message_is_plain_ascii_and_names_the_title() {
        let message = resume_startup_message("Fix login bug");
        assert_eq!(
            message,
            "banto: resuming \"Fix login bug\" ... \
             (returns to the list when the session closes)"
        );
        assert!(message.is_ascii());
    }

    #[test]
    fn new_session_startup_message_is_plain_ascii_and_names_the_cwd() {
        let message = new_session_startup_message(Path::new("/work/alpha"));
        assert_eq!(message, "banto: starting a new session in /work/alpha ...");
        assert!(message.is_ascii());
    }

    #[test]
    fn decide_inplace_resume_proceeds_when_the_session_is_not_live() {
        let probe = MockProbe::with_alive(&[]);

        let launch =
            decide_inplace_resume(&session(), &probe, &[], &AgentBinaries::default()).unwrap();

        assert_eq!(
            launch.argv,
            ["claude", "--resume", "sess-1"].map(str::to_string)
        );
        assert_eq!(launch.cwd, PathBuf::from("/work/alpha"));
        assert_eq!(launch.startup_message, resume_startup_message("Fix login"));
    }

    #[test]
    fn decide_inplace_resume_codex_session_resumes_via_codex() {
        let probe = MockProbe::with_alive(&[]);
        let mut codex_session = session();
        codex_session.agent = AgentKind::Codex;

        let launch =
            decide_inplace_resume(&codex_session, &probe, &[], &AgentBinaries::default()).unwrap();

        assert_eq!(
            launch.argv,
            ["codex", "resume", "sess-1", "-C", "/work/alpha"].map(str::to_string)
        );
    }

    #[test]
    fn decide_inplace_resume_refuses_when_the_session_is_already_live() {
        let probe = MockProbe::with_alive(&[4242]);
        let live = [live_entry("sess-1", 4242)];

        assert_eq!(
            decide_inplace_resume(&session(), &probe, &live, &AgentBinaries::default()),
            None
        );
    }

    #[test]
    fn decide_inplace_resume_ignores_a_live_entry_for_a_different_session() {
        let probe = MockProbe::with_alive(&[4242]);
        let live = [live_entry("some-other-session", 4242)];

        assert!(
            decide_inplace_resume(&session(), &probe, &live, &AgentBinaries::default()).is_some()
        );
    }

    #[test]
    fn live_but_untracked_session_refuses_to_open_fresh_to_avoid_a_double_resume() {
        let store = Store::open_in_memory().unwrap();
        let probe = MockProbe::with_alive(&[4242]);
        let runner = MockRunner::default();
        let live = [live_entry("sess-1", 4242)];

        let outcome = open_session(
            &store,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &OpenContext {
                probe: &probe,
                live: &live,
                binaries: &AgentBinaries::default(),
            },
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::AlreadyRunningUntracked);
        // Never attempted to open a second pane, and never recorded one.
        assert!(runner.calls().is_empty());
        assert_eq!(
            store.get_pane(&SessionId("sess-1".to_string())).unwrap(),
            None
        );
    }

    #[test]
    fn untracked_session_with_no_matching_live_entry_opens_fresh_as_before() {
        let store = Store::open_in_memory().unwrap();
        // A live entry exists, but for a different session — and one for
        // this session's id whose pid is dead. Neither should block a fresh
        // open.
        let probe = MockProbe::with_alive(&[100]);
        let runner = MockRunner::default();
        let live = [
            live_entry("some-other-session", 100),
            live_entry("sess-1", 999), // dead pid
        ];

        let outcome = open_session(
            &store,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &OpenContext {
                probe: &probe,
                live: &live,
                binaries: &AgentBinaries::default(),
            },
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
    }

    #[test]
    fn open_fresh_anchors_the_split_on_the_given_pane() {
        let store = Store::open_in_memory().unwrap();
        let probe = MockProbe::with_alive(&[]);
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            Some("%7"),
            &OpenContext {
                probe: &probe,
                live: &[],
                binaries: &AgentBinaries::default(),
            },
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let calls = runner.calls();
        let args = &calls[0].args;
        assert_eq!(args[0], "split-window");
        let t = args.iter().position(|a| a == "-t").expect("-t missing");
        assert_eq!(args[t + 1], "%7");
    }

    #[test]
    fn no_backend_detected_opens_nothing() {
        let store = Store::open_in_memory().unwrap();
        let probe = MockProbe::with_alive(&[]);
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            None,
            &session(),
            runner.clone(),
            None,
            &OpenContext {
                probe: &probe,
                live: &[],
                binaries: &AgentBinaries::default(),
            },
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::NoBackendDetected);
        assert!(runner.calls().is_empty());
        assert_eq!(
            store.get_pane(&SessionId("sess-1".to_string())).unwrap(),
            None
        );
    }

    #[test]
    fn focuses_existing_pane_when_pid_is_alive() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "psmux".to_string(),
                target: "play:@3:%8".to_string(),
                pid: Some(100),
                opened_at: SystemTime::now(),
            })
            .unwrap();
        let probe = MockProbe::with_alive(&[100]);
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &OpenContext {
                probe: &probe,
                live: &[],
                binaries: &AgentBinaries::default(),
            },
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Focused);
        let calls = runner.calls();
        // Only a session-qualified `select-pane` (docs/notes/psmux-spike.md,
        // 2026-07-20): no `select-window` (fails on psmux for `session:@id`
        // targets) and no `switch-client` (corrupted the live server).
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args, vec!["select-pane", "-t", "play:%8"]);
    }

    #[test]
    fn missing_pid_is_treated_as_alive_to_avoid_double_resume() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "psmux".to_string(),
                target: "play:@3:%8".to_string(),
                pid: None,
                opened_at: SystemTime::now(),
            })
            .unwrap();
        let probe = MockProbe::with_alive(&[]);
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &OpenContext {
                probe: &probe,
                live: &[],
                binaries: &AgentBinaries::default(),
            },
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Focused);
    }

    #[test]
    fn dead_pid_is_stale_and_opens_fresh() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "psmux".to_string(),
                target: "play:@3:%8".to_string(),
                pid: Some(999),
                opened_at: SystemTime::now(),
            })
            .unwrap();
        let probe = MockProbe::with_alive(&[]); // 999 is dead
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &OpenContext {
                probe: &probe,
                live: &[],
                binaries: &AgentBinaries::default(),
            },
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let record = store
            .get_pane(&SessionId("sess-1".to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(record.target, "play:@1:%1");
    }

    #[test]
    fn focus_command_error_is_treated_as_stale_and_opens_fresh() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "psmux".to_string(),
                target: "play:@3:%8".to_string(),
                pid: Some(100),
                opened_at: SystemTime::now(),
            })
            .unwrap();
        // Pid alive, so a normal focus would be attempted, but the backend
        // reports the pane itself is gone (e.g. closed out of band).
        let probe = MockProbe::with_alive(&[100]);
        let runner = MockRunner::failing_select_pane();

        let outcome = open_session(
            &store,
            Some(Backend::Psmux),
            &session(),
            runner.clone(),
            None,
            &OpenContext {
                probe: &probe,
                live: &[],
                binaries: &AgentBinaries::default(),
            },
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let record = store
            .get_pane(&SessionId("sess-1".to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(record.target, "play:@1:%1");
        assert_ne!(record.target, "play:@3:%8");
    }

    #[test]
    fn windows_terminal_alive_session_refuses_a_second_open() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_pane(&PaneRecord {
                session_id: SessionId("sess-1".to_string()),
                backend: "windows-terminal".to_string(),
                target: String::new(),
                pid: Some(100),
                opened_at: SystemTime::now(),
            })
            .unwrap();
        let probe = MockProbe::with_alive(&[100]);
        let runner = MockRunner::default();

        let outcome = open_session(
            &store,
            Some(Backend::WindowsTerminal),
            &session(),
            runner.clone(),
            None,
            &OpenContext {
                probe: &probe,
                live: &[],
                binaries: &AgentBinaries::default(),
            },
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::AlreadyOpenCannotFocus);
        // Never attempted to open a second tab.
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn open_new_session_wraps_claude_via_wrap_new_session_with_no_session_id() {
        let runner = MockRunner::default();

        let outcome = open_new_session(
            Some(Backend::Psmux),
            &PathBuf::from("/work/alpha"),
            runner.clone(),
            None,
            None,
            &AgentBinaries::default(),
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::Opened);
        let calls = runner.calls();
        // Wrapped via `_wrap --new-session --cwd ... -- claude` (no
        // `--session <id>`: there's no pre-existing session id to tell it).
        assert!(calls[0].args.iter().any(|a| a == "_wrap"));
        assert!(calls[0].args.iter().any(|a| a == "--new-session"));
        assert!(!calls[0].args.iter().any(|a| a == "--session"));
        // No `BANTO_WRAP_LOG` was resolved, so no `--wrap-log` flag either.
        assert!(!calls[0].args.iter().any(|a| a == "--wrap-log"));
        let cwd_pos = calls[0]
            .args
            .iter()
            .position(|a| a == "--cwd")
            .expect("--cwd missing");
        assert_eq!(calls[0].args[cwd_pos + 1], "/work/alpha");
        let backend_pos = calls[0]
            .args
            .iter()
            .position(|a| a == "--backend")
            .expect("--backend missing");
        assert_eq!(calls[0].args[backend_pos + 1], "psmux");
        assert!(calls[0].args.iter().any(|a| a == "claude"));
        // Tagged with the cwd's directory name, not a session id.
        assert_eq!(
            calls[1].args,
            vec!["select-pane", "-t", "play:%1", "-T", "alpha"]
        );
    }

    #[test]
    fn open_new_session_anchors_the_split_when_given_an_anchor_pane() {
        let runner = MockRunner::default();

        open_new_session(
            Some(Backend::Psmux),
            &PathBuf::from("/work/alpha"),
            runner.clone(),
            Some("%7"),
            None,
            &AgentBinaries::default(),
        )
        .unwrap();

        let calls = runner.calls();
        let t = calls[0]
            .args
            .iter()
            .position(|a| a == "-t")
            .expect("-t missing");
        assert_eq!(calls[0].args[t + 1], "%7");
    }

    #[test]
    fn open_new_session_title_falls_back_to_the_full_path_without_a_file_name() {
        let runner = MockRunner::default();

        open_new_session(
            Some(Backend::Psmux),
            &PathBuf::from("/"),
            runner.clone(),
            None,
            None,
            &AgentBinaries::default(),
        )
        .unwrap();

        let calls = runner.calls();
        assert_eq!(
            calls[1].args,
            vec!["select-pane", "-t", "play:%1", "-T", "/"]
        );
    }

    #[test]
    fn open_new_session_with_no_backend_reports_no_backend_detected() {
        let runner = MockRunner::default();

        let outcome = open_new_session(
            None,
            &PathBuf::from("/work/alpha"),
            runner.clone(),
            None,
            None,
            &AgentBinaries::default(),
        )
        .unwrap();

        assert_eq!(outcome, OpenOutcome::NoBackendDetected);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn open_new_session_passes_wrap_log_through_to_the_wrap_argv() {
        let runner = MockRunner::default();

        open_new_session(
            Some(Backend::Psmux),
            &PathBuf::from("/work/alpha"),
            runner.clone(),
            None,
            Some("C:/logs/wrap.log"),
            &AgentBinaries::default(),
        )
        .unwrap();

        let calls = runner.calls();
        let pos = calls[0]
            .args
            .iter()
            .position(|a| a == "--wrap-log")
            .expect("--wrap-log missing");
        assert_eq!(calls[0].args[pos + 1], "C:/logs/wrap.log");
    }

    // --- encode_target / decode_handle -----------------------------------

    #[test]
    fn encode_decode_tmux_target_roundtrips_the_session_qualified_form() {
        let handle = SessionHandle::Tmux {
            session: "play".to_string(),
            window_id: "@3".to_string(),
            pane_id: "%8".to_string(),
        };

        let target = encode_target(&handle);

        assert_eq!(target, "play:@3:%8");
        assert_eq!(decode_handle(Backend::Psmux, &target), Some(handle));
    }

    #[test]
    fn decode_handle_rejects_a_pre_session_qualification_two_part_target() {
        // See decode_handle's doc — must not be misinterpreted as
        // `<session>:<pane_id>` with a missing pane.
        assert_eq!(decode_handle(Backend::Psmux, "@3:%8"), None);
    }

    #[test]
    fn encode_target_windows_terminal_is_empty() {
        assert_eq!(encode_target(&SessionHandle::WindowsTerminal), "");
    }

    #[test]
    fn decode_handle_windows_terminal_ignores_the_target_string() {
        assert_eq!(
            decode_handle(Backend::WindowsTerminal, ""),
            Some(SessionHandle::WindowsTerminal)
        );
    }

    // --- resolve_own_anchor / resolve_own_session -------------------------

    #[test]
    fn resolve_own_anchor_queries_the_session_name_and_qualifies_the_pane() {
        let runner = MockRunner::with_target_responses([CommandOutput::success("play\n")]);

        let anchor = resolve_own_anchor(Some(Backend::Psmux), &runner, Some("%2"));

        assert_eq!(anchor.as_deref(), Some("play:%2"));
        assert_eq!(
            runner.calls(),
            vec![CommandSpec::new(
                "psmux",
                ["display-message", "-p", "#{session_name}"].map(str::to_string)
            )]
        );
    }

    #[test]
    fn resolve_own_anchor_falls_back_to_the_bare_pane_when_session_lookup_fails() {
        let runner = MockRunner::with_target_responses([CommandOutput::failure(
            Some(1),
            "no server running".to_string(),
        )]);

        let anchor = resolve_own_anchor(Some(Backend::Psmux), &runner, Some("%2"));

        assert_eq!(anchor.as_deref(), Some("%2"));
    }

    #[test]
    fn resolve_own_anchor_is_none_without_a_tmux_pane() {
        let runner = MockRunner::with_target_responses([CommandOutput::success("play\n")]);

        assert_eq!(
            resolve_own_anchor(Some(Backend::Psmux), &runner, None),
            None
        );
        // Never shells out when there's no pane to qualify in the first place.
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn resolve_own_anchor_is_none_for_a_non_psmux_backend_even_with_tmux_pane_set() {
        // Mirrors `crate::wrap::resolve_own_pane`'s reasoning: banto could be
        // running inside an unrelated psmux session (so `$TMUX_PANE` is set)
        // while still configured to open new sessions via Windows Terminal.
        let runner = MockRunner::with_target_responses([CommandOutput::success("play\n")]);

        assert_eq!(
            resolve_own_anchor(Some(Backend::WindowsTerminal), &runner, Some("%2")),
            None
        );
        assert_eq!(resolve_own_anchor(None, &runner, Some("%2")), None);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn wrap_argv_uses_the_given_exe_path_when_available() {
        assert_eq!(
            wrap_argv(
                Some("C:/dev/banto.exe"),
                "sess-1",
                AgentKind::ClaudeCode,
                Path::new("/work/alpha"),
                &AgentBinaries::default(),
            ),
            [
                "C:/dev/banto.exe",
                "_wrap",
                "--session",
                "sess-1",
                "--",
                "claude",
                "--resume",
                "sess-1",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn wrap_argv_falls_back_to_the_bare_name_without_an_exe_path() {
        assert_eq!(
            wrap_argv(
                None,
                "sess-1",
                AgentKind::ClaudeCode,
                Path::new("/work/alpha"),
                &AgentBinaries::default(),
            )[0],
            "banto"
        );
    }

    #[test]
    fn wrap_argv_codex_session_resumes_via_codex_with_the_cwd_flag() {
        assert_eq!(
            wrap_argv(
                None,
                "codex-uuid-1",
                AgentKind::Codex,
                Path::new("/work/alpha"),
                &AgentBinaries::default(),
            ),
            [
                "banto",
                "_wrap",
                "--session",
                "codex-uuid-1",
                "--",
                "codex",
                "resume",
                "codex-uuid-1",
                "-C",
                "/work/alpha",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn new_session_wrap_argv_uses_the_given_exe_path_when_available() {
        assert_eq!(
            new_session_wrap_argv(
                Some("C:/dev/banto.exe"),
                Path::new("/work/alpha"),
                Backend::Psmux,
                None,
                &AgentBinaries::default(),
            ),
            [
                "C:/dev/banto.exe",
                "_wrap",
                "--new-session",
                "--cwd",
                "/work/alpha",
                "--backend",
                "psmux",
                "--",
                "claude",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn new_session_wrap_argv_falls_back_to_the_bare_name_without_an_exe_path() {
        assert_eq!(
            new_session_wrap_argv(
                None,
                Path::new("/work/alpha"),
                Backend::Psmux,
                None,
                &AgentBinaries::default(),
            )[0],
            "banto"
        );
    }

    #[test]
    fn new_session_wrap_argv_passes_the_windows_terminal_backend_key() {
        let argv = new_session_wrap_argv(
            None,
            Path::new("/work/alpha"),
            Backend::WindowsTerminal,
            None,
            &AgentBinaries::default(),
        );
        let pos = argv
            .iter()
            .position(|a| a == "--backend")
            .expect("--backend missing");
        assert_eq!(argv[pos + 1], "windows-terminal");
    }

    #[test]
    fn new_session_wrap_argv_appends_wrap_log_before_the_trailing_claude_argv() {
        let argv = new_session_wrap_argv(
            None,
            Path::new("/work/alpha"),
            Backend::Psmux,
            Some("C:/logs/wrap.log"),
            &AgentBinaries::default(),
        );
        let pos = argv
            .iter()
            .position(|a| a == "--wrap-log")
            .expect("--wrap-log missing");
        assert_eq!(argv[pos + 1], "C:/logs/wrap.log");
        // Still ends with the separator and the wrapped command.
        assert_eq!(&argv[pos + 2..], ["--", "claude"]);
    }

    #[test]
    fn new_session_wrap_argv_omits_wrap_log_when_not_given() {
        let argv = new_session_wrap_argv(
            None,
            Path::new("/work/alpha"),
            Backend::Psmux,
            None,
            &AgentBinaries::default(),
        );
        assert!(!argv.iter().any(|a| a == "--wrap-log"));
    }

    #[test]
    fn agent_binary_is_used_as_argv0_when_configured() {
        let binaries = AgentBinaries {
            claude: None,
            codex: Some(PathBuf::from("C:/tools/codex.exe")),
        };
        let argv = wrap_argv(
            None,
            "codex-uuid-1",
            AgentKind::Codex,
            Path::new("/work/alpha"),
            &binaries,
        );
        assert_eq!(argv.last(), Some(&"/work/alpha".to_string()));
        // The wrapped command's own argv[0] (after the `--` separator) is
        // the configured Codex binary, not the bare "codex" default.
        let sep = argv.iter().position(|a| a == "--").expect("-- missing");
        assert_eq!(argv[sep + 1], "C:/tools/codex.exe");
    }

    #[test]
    fn resolve_backend_forces_configured_backend_regardless_of_env() {
        assert_eq!(
            resolve_backend(OpenerMode::Psmux, |_| Some("set".to_string()), false),
            Some(Backend::Psmux)
        );
        assert_eq!(
            resolve_backend(OpenerMode::WindowsTerminal, |_| None, false),
            Some(Backend::WindowsTerminal)
        );
    }

    #[test]
    fn resolve_backend_auto_detects_from_env() {
        let in_tmux = |k: &str| (k == "TMUX").then(|| "1".to_string());
        // `$TMUX` says "inside a multiplexer", not which one: real tmux off
        // Windows, psmux on it.
        assert_eq!(
            resolve_backend(OpenerMode::Auto, in_tmux, false),
            Some(Backend::Tmux)
        );
        assert_eq!(
            resolve_backend(OpenerMode::Auto, in_tmux, true),
            Some(Backend::Psmux)
        );
        assert_eq!(resolve_backend(OpenerMode::Auto, |_| None, false), None);
    }

    #[test]
    fn resolve_backend_in_place_falls_back_to_auto_detection_for_the_split_key() {
        // `InPlace` has no split backend of its own; the `s` key still needs
        // one, so it detects from the environment exactly like `Auto`.
        assert_eq!(
            resolve_backend(
                OpenerMode::InPlace,
                |k| (k == "TMUX").then(|| "1".to_string()),
                false
            ),
            Some(Backend::Tmux)
        );
        assert_eq!(resolve_backend(OpenerMode::InPlace, |_| None, false), None);
    }
}
