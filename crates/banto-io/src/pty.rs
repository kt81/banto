//! PTY host abstraction: spawn a child in a pseudo-terminal and expose its
//! output stream, an input sink, and a resize handle. Behind a trait so tests
//! never spawn a real process (CLAUDE.md: every external process invocation
//! sits behind a mockable abstraction).

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use anyhow::Result;
use banto_core::model::PtyExitReason;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Everything a hosted child is reachable by: its output chunks, an input
/// sink, a resize handle, a kill handle, its pid, and an exit signal.
/// Returned by [`PtyHost::open`].
pub struct PtyIo {
    /// Chunks of the child's terminal output, pumped from a reader thread.
    pub output: Receiver<Vec<u8>>,
    /// Writes here go to the child's stdin.
    pub input: Box<dyn Write + Send>,
    /// Resizes the child's PTY (and keeps the child process alive).
    pub resizer: Box<dyn Resizer>,
    /// Kills the child (`Cmd::KillPty`'s executor).
    pub killer: Box<dyn Killer>,
    /// Asks the child to shut down as if its terminal window closed — what
    /// dropping the master alone fails to convey on Unix. See [`Hangup`].
    pub hangup: Box<dyn Hangup>,
    /// OS process id of the direct child, when the platform reports one.
    /// `claude` writes `<claude_home>/sessions/<pid>.json` at startup, so
    /// this is what lets a freshly-spawned session be identified before it
    /// has written any session history (see
    /// `banto::embedded::emporium::poll_discovery`). `None` whenever the
    /// direct child isn't the `claude` process itself — a Windows `.cmd`
    /// shim spawned through a shell, say — in which case id discovery falls
    /// back to matching session files by cwd.
    pub pid: Option<u32>,
    /// Fires exactly once, when the child has actually exited, carrying
    /// what `Child::wait()` reported (translated to banto-core's own plain
    /// [`PtyExitReason`] here, at the I/O boundary — see that type's own
    /// doc for why the richer `portable_pty::ExitStatus` never leaves this
    /// crate). On ConPTY, a child's exit does **not** produce EOF on
    /// `output` (the pseudoconsole keeps the pipe open), so `output`
    /// disconnecting is a Unix-only signal — this channel is the active,
    /// cross-platform one. See `PortablePtyHost::open`'s exit-waiter thread
    /// and, in the `banto` crate, `embedded::session::PtyHandle::poll`'s
    /// doc for how the two combine.
    pub exited: Receiver<PtyExitReason>,
}

/// Resizes a hosted child's PTY.
pub trait Resizer: Send {
    fn resize(&self, rows: u16, cols: u16) -> Result<()>;
}

/// Kills a hosted child's process.
pub trait Killer: Send {
    fn kill(&mut self) -> Result<()>;
}

/// Tells a hosted child that its terminal went away — the polite "your
/// window just closed" every terminal app already knows how to handle,
/// short of a kill.
///
/// Exists because the two platforms disagree about what closing the master
/// *means*, and only one of them says it out loud:
///
/// - **Windows.** Dropping the master closes the pseudoconsole, which
///   raises the console-close cascade (`CTRL_CLOSE_EVENT`) at the child.
///   Nothing more is needed, so the implementation is a no-op.
/// - **Unix.** The tty driver hangs up (SIGHUP to the foreground process
///   group) only when the *last* fd on the master closes — and one is held
///   by the reader thread, parked in a `read()` that will not return until
///   that very hangup happens. Dropping banto's own copies is therefore
///   silent: the child keeps running, oblivious, until something
///   force-kills it. Measured on WSL 2026-07-25: an embedded `claude` with
///   the reader's clone still open sat through the whole 5s shutdown grace
///   and died by `SIGKILL`; sent the hangup explicitly, it exits cleanly in
///   ~0.5s. So the signal the tty would have sent is sent by hand.
pub trait Hangup: Send {
    fn hangup(&mut self) -> Result<()>;
}

/// Environment variables banto strips from every agent process it spawns,
/// on every launch route (embedded PTY, in-place, `_wrap`) — see each
/// [`PtyHost::open`]/`ProcessRunner` call site for where this is threaded
/// through.
///
/// `CLAUDE_CODE_CHILD_SESSION` is the one entry: measured on this machine
/// against `claude` 2.1.222, 2026-08-05 — inherited (present, value `"1"`)
/// by a bare `claude` launch (no `--resume`/`--session-id`), it wrote no
/// `sessions/<pid>.json` for its entire observed life (boot through two
/// completed turns, zero appearances polled at 1s resolution); the same
/// variable present, `claude --session-id <uuid>` still wrote one normally.
/// The boundary is the launch shape, not the variable's mere presence —
/// every fresh (non-`--resume`) launch banto itself makes is exactly the
/// shape that loses the file, so stripping this one variable removes an
/// inherited-but-false signal at the point it actually changes behavior.
/// banto's own code never reads this variable (repo-wide search, 0 hits) —
/// stripping it can only change what the *child* does with it.
pub const STRIPPED_CHILD_ENV_VARS: &[&str] = &["CLAUDE_CODE_CHILD_SESSION"];

/// Spawns a child inside a PTY.
///
/// `env` is *added* to the environment banto itself is running under, not a
/// replacement for it — a child agent needs the operator's own `PATH`, home
/// and terminal settings to behave like one they started themselves.
/// `env_remove` is the opposite: entries stripped from that same inherited
/// environment before the child ever sees them (see
/// [`STRIPPED_CHILD_ENV_VARS`]) — carried as its own parameter, not folded
/// into `env`, so [`mock::MockPtyHost`] can record it independently of the
/// additions list.
pub trait PtyHost {
    fn open(
        &self,
        argv: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
        env_remove: &[&str],
        rows: u16,
        cols: u16,
    ) -> Result<PtyIo>;
}

/// [`PtyHost`] backed by `portable-pty-psmux` (ConPTY on Windows, a Unix pty
/// elsewhere) — a fork of `portable-pty`, not the crates.io original: it
/// requests `PSEUDOCONSOLE_PASSTHROUGH_MODE` on Windows 11 22H2+ (build
/// ≥22621), which makes ConPTY relay a child's VT bytes verbatim instead of
/// reinterpreting them through its own legacy Win32 console buffer and
/// re-serializing its own reconstruction. Without it, a child that reserves
/// a footer via DECSTBM (Codex does) never gets a single line into
/// `vt100`'s scrollback, because ConPTY's own re-serialization uses a
/// narrow scroll region unconditionally — measured 2026-08-02 with a
/// controlled A/B (same Codex binary, same prompt, only the PTY crate
/// swapped): scrollback stayed 0 without this flag, reached 191 with it.
#[derive(Debug, Default, Clone, Copy)]
pub struct PortablePtyHost;

impl PtyHost for PortablePtyHost {
    fn open(
        &self,
        argv: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
        env_remove: &[&str],
        rows: u16,
        cols: u16,
    ) -> Result<PtyIo> {
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(&argv[0]);
        for arg in &argv[1..] {
            cmd.arg(arg);
        }
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        for (key, value) in env {
            cmd.env(key, value);
        }
        // `CommandBuilder::new` above already seeded `cmd`'s environment
        // from banto's own (`portable-pty-psmux` 0.9.6,
        // `cmdbuilder.rs::get_base_env`), so this removes a single key from
        // that inherited base — not a rebuild-from-scratch, and independent
        // of the additions loop just above.
        for key in env_remove {
            cmd.env_remove(key);
        }
        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        // Both read before `child` moves into the exit-waiter thread below:
        // a `ChildKiller` is an independent handle to the same process, so
        // the resize/kill capabilities don't need to fight over one
        // `&mut Child`, and the pid is a plain value.
        let killer = child.clone_killer();
        let pid = child.process_id();

        let mut reader = pair.master.try_clone_reader()?;
        let input = pair.master.take_writer()?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // ConPTY quirk: a child's exit does not produce EOF on the master
        // reader (see `PtyIo::exited`'s doc), so the read loop above would
        // block forever and never report it. This thread actively waits
        // instead — `child.wait()` blocks until the process truly exits on
        // every platform, so it also does what holding `child` here used to
        // do (keep the handle alive for the pane's lifetime); the resizer
        // holder below now keeps only the master.
        let (exited_tx, exited_rx) = mpsc::channel::<PtyExitReason>();
        std::thread::spawn(move || {
            let _ = exited_tx.send(pty_exit_reason(child.wait()));
        });

        Ok(PtyIo {
            output: rx,
            input,
            resizer: Box::new(PortablePtyResizer {
                master: pair.master,
            }),
            killer: Box::new(PortablePtyKiller(killer)),
            hangup: new_hangup(pid),
            pid,
            exited: exited_rx,
        })
    }
}

/// Translate `Child::wait()`'s own result into banto-core's plain
/// [`PtyExitReason`] — the I/O-crate side of the boundary that type's own
/// doc describes. `portable_pty::ExitStatus` carries a `signal` only on
/// Unix (`ExitStatus::from(std::process::ExitStatus)`, `pty.rs`'s upstream
/// conversion); a status with none is reported by exit code alone,
/// regardless of platform. `wait()` itself returning `Err` — observed on no
/// real machine so far, but the trait allows it — is not treated as "still
/// running": the exit-waiter thread only ever reaches this after `wait()`
/// has returned at all, so the child is known gone, just not how.
fn pty_exit_reason(result: std::io::Result<portable_pty::ExitStatus>) -> PtyExitReason {
    match result {
        Ok(status) => match status.signal() {
            Some(signal) => PtyExitReason::Signal(signal.to_string()),
            None => PtyExitReason::Code(status.exit_code()),
        },
        Err(_) => PtyExitReason::Unknown,
    }
}

#[cfg(test)]
mod pty_exit_reason_tests {
    use portable_pty::ExitStatus;

    use super::pty_exit_reason;
    use banto_core::model::PtyExitReason;

    #[test]
    fn a_zero_exit_code_maps_to_code_zero() {
        assert_eq!(
            pty_exit_reason(Ok(ExitStatus::with_exit_code(0))),
            PtyExitReason::Code(0)
        );
    }

    #[test]
    fn a_nonzero_exit_code_maps_to_that_code() {
        assert_eq!(
            pty_exit_reason(Ok(ExitStatus::with_exit_code(42))),
            PtyExitReason::Code(42)
        );
    }

    #[test]
    fn a_signal_maps_to_signal_by_name_regardless_of_its_own_code() {
        assert_eq!(
            pty_exit_reason(Ok(ExitStatus::with_signal("SIGKILL"))),
            PtyExitReason::Signal("SIGKILL".to_string())
        );
    }

    #[test]
    fn a_wait_error_maps_to_unknown_not_treated_as_still_running() {
        assert_eq!(
            pty_exit_reason(Err(std::io::Error::other("wait() failed"))),
            PtyExitReason::Unknown
        );
    }
}

/// Unix: the hangup the tty driver won't deliver while the reader thread
/// holds its clone of the master (see [`Hangup`]).
#[cfg(unix)]
fn new_hangup(pid: Option<u32>) -> Box<dyn Hangup> {
    Box::new(SignalHangup(pid))
}

/// Everywhere else (Windows): dropping the master already is the hangup.
#[cfg(not(unix))]
fn new_hangup(_pid: Option<u32>) -> Box<dyn Hangup> {
    Box::new(NoHangup)
}

#[cfg(unix)]
struct SignalHangup(Option<u32>);

#[cfg(unix)]
impl Hangup for SignalHangup {
    fn hangup(&mut self) -> Result<()> {
        // `pid > 1` is a hard guard, not superstition: `killpg(0, ...)`
        // signals the *caller's* own process group — banto and every pane
        // it hosts, plus the shell that launched it.
        let Some(pid) = self.0.filter(|pid| *pid > 1) else {
            return Ok(());
        };
        // The child is a session leader owning the pty as its controlling
        // terminal (portable-pty does setsid + TIOCSCTTY), so its pid is
        // also its process-group id. Signalling the group — not the lone
        // pid — is what the tty driver itself does on hangup, and it
        // reaches whatever the session spawned (its `banto _mcp` server)
        // instead of orphaning it.
        if unsafe { libc::killpg(pid as i32, libc::SIGHUP) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

#[cfg(not(unix))]
struct NoHangup;

#[cfg(not(unix))]
impl Hangup for NoHangup {
    fn hangup(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Holds the master PTY, for resizing.
struct PortablePtyResizer {
    master: Box<dyn MasterPty + Send>,
}

impl Resizer for PortablePtyResizer {
    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
    }
}

struct PortablePtyKiller(Box<dyn ChildKiller + Send + Sync>);

impl Killer for PortablePtyKiller {
    fn kill(&mut self) -> Result<()> {
        self.0.kill().map_err(Into::into)
    }
}

/// Not `#[cfg(test)]`: the `banto` crate's `embedded::session` and
/// `embedded::emporium` (a *different* crate) both need this for their own
/// tests, and `#[cfg(test)]` is crate-local in Rust — it never survives
/// across a crate boundary, even into a downstream crate's own test build.
/// Always compiled instead (harmless: nothing in a real build path ever
/// references it, so it never reaches the linked binary).
pub mod mock {
    use std::io::{self, Write};
    use std::path::Path;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use banto_core::model::PtyExitReason;

    use super::{Hangup, Killer, PtyHost, PtyIo, Resizer};

    /// A [`PtyHost`] that spawns nothing: it replays `script` as the child's
    /// output and records everything written to the child, every resize, and
    /// every kill. `open` stashes the fresh `exited` sender in `exit_sender`
    /// so a test can fire it later via [`Self::fire_exit`], simulating the
    /// real waiter thread's `child.wait()` returning.
    #[derive(Default)]
    pub struct MockPtyHost {
        pub script: Vec<u8>,
        pub captured: Arc<Mutex<Vec<u8>>>,
        pub resizes: Arc<Mutex<Vec<(u16, u16)>>>,
        pub kills: Arc<Mutex<u32>>,
        /// Counts [`super::Hangup::hangup`] calls. Deliberately does *not*
        /// fire `exited` the way [`MockKiller`] does: a hangup is a request,
        /// and a child free to ignore it is exactly the case the shutdown
        /// sweep's deadline exists for.
        pub hangups: Arc<Mutex<u32>>,
        /// Set by `open`; not meant to be set by test setup directly (use
        /// [`Self::fire_exit`]) — `pub` only because struct-update syntax
        /// (`..Default::default()`) requires every field to be nameable
        /// from the construction site, even ones taken from `Default`.
        pub exit_sender: Arc<Mutex<Option<mpsc::Sender<PtyExitReason>>>>,
        /// Reported as the spawned child's pid (real hosts read it from the
        /// OS). `None` by default, matching a platform that can't report
        /// one.
        pub pid: Option<u32>,
        /// When `true`, `open`'s output sender drops as soon as `script` is
        /// sent — simulating a real Unix PTY's reader thread hitting EOF on
        /// child exit, independent of `exited`. When `false` (the default),
        /// the sender is kept alive on a parked background thread, so
        /// `output` stays `Empty` forever on its own — matching ConPTY,
        /// where a child's exit never closes this side by itself; only
        /// `fire_exit`/`kill` (via `exited`) ever end the session.
        pub unix_style_exit: bool,
        /// Every `env_remove` list `open` was called with, in call order —
        /// unlike `argv`/`cwd`/`env`, which this mock still discards, this
        /// one is recorded so a test can catch a call site silently
        /// dropping [`super::STRIPPED_CHILD_ENV_VARS`] (a change that would
        /// otherwise pass every gate in this repo and be caught only by a
        /// real re-measurement — see that constant's own doc for why it
        /// matters).
        pub env_removes: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl MockPtyHost {
        /// Simulate the child exiting cleanly (`PtyExitReason::Code(0)`):
        /// fires the `exited` channel of the most recently opened `PtyIo`. A
        /// no-op if nothing is open, or the exit was already fired (a real
        /// exit only ever happens once). Use
        /// [`Self::fire_exit_with_reason`] for a test that cares which
        /// reason lands.
        pub fn fire_exit(&self) {
            self.fire_exit_with_reason(PtyExitReason::Code(0));
        }

        /// Same as [`Self::fire_exit`], but with the exact reason a test
        /// wants observed downstream (a nonzero code, a signal, `Unknown`).
        pub fn fire_exit_with_reason(&self, reason: PtyExitReason) {
            if let Some(tx) = self.exit_sender.lock().unwrap().take() {
                let _ = tx.send(reason);
            }
        }
    }

    impl PtyHost for MockPtyHost {
        fn open(
            &self,
            _argv: &[String],
            _cwd: Option<&Path>,
            _env: &[(String, String)],
            env_remove: &[&str],
            _rows: u16,
            _cols: u16,
        ) -> Result<PtyIo> {
            self.env_removes
                .lock()
                .unwrap()
                .push(env_remove.iter().map(|s| s.to_string()).collect());
            let (tx, rx) = mpsc::channel();
            // Sent synchronously, before `open` returns: a caller's very
            // first `poll()` must see it, not race a background thread.
            if !self.script.is_empty() {
                let _ = tx.send(self.script.clone());
            }
            if self.unix_style_exit {
                drop(tx); // disconnects immediately, like a real Unix EOF.
            } else {
                // ConPTY: keep `tx` alive forever on a parked thread, so
                // `output` stays `Empty` (never disconnects on its own).
                std::thread::spawn(move || {
                    let _tx = tx;
                    std::thread::park();
                });
            }
            let (exited_tx, exited_rx) = mpsc::channel();
            *self.exit_sender.lock().unwrap() = Some(exited_tx);
            Ok(PtyIo {
                output: rx,
                input: Box::new(CapturingWriter(self.captured.clone())),
                resizer: Box::new(MockResizer(self.resizes.clone())),
                killer: Box::new(MockKiller(self.kills.clone(), self.exit_sender.clone())),
                hangup: Box::new(MockHangup(self.hangups.clone())),
                pid: self.pid,
                exited: exited_rx,
            })
        }
    }

    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct MockResizer(Arc<Mutex<Vec<(u16, u16)>>>);

    impl Resizer for MockResizer {
        fn resize(&self, rows: u16, cols: u16) -> Result<()> {
            self.0.lock().unwrap().push((rows, cols));
            Ok(())
        }
    }

    struct MockHangup(Arc<Mutex<u32>>);

    impl Hangup for MockHangup {
        fn hangup(&mut self) -> Result<()> {
            *self.0.lock().unwrap() += 1;
            Ok(())
        }
    }

    /// Mirrors reality: killing a child makes it exit, so `kill` also fires
    /// the same `exited` signal a real `child.wait()` returning would —
    /// keeps `session.rs`'s `poll_reaches_disconnected_after_kill` honest.
    /// The reason sent is `Unknown`: a real kill on Unix is a signal (which
    /// platform-specific one depends on `Killer::kill`'s own implementation,
    /// not modeled here), and on Windows a forced termination reports
    /// through the same `ExitStatus` a graceful exit does — nothing this
    /// mock can say with a straight face beyond "gone, not by its own
    /// choosing."
    struct MockKiller(
        Arc<Mutex<u32>>,
        Arc<Mutex<Option<mpsc::Sender<PtyExitReason>>>>,
    );

    impl Killer for MockKiller {
        fn kill(&mut self) -> Result<()> {
            *self.0.lock().unwrap() += 1;
            if let Some(tx) = self.1.lock().unwrap().take() {
                let _ = tx.send(PtyExitReason::Unknown);
            }
            Ok(())
        }
    }
}
