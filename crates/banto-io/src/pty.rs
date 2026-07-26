//! PTY host abstraction: spawn a child in a pseudo-terminal and expose its
//! output stream, an input sink, and a resize handle. Behind a trait so tests
//! never spawn a real process (CLAUDE.md: every external process invocation
//! sits behind a mockable abstraction).

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use anyhow::Result;
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
    /// Fires exactly once, when the child has actually exited. On ConPTY, a
    /// child's exit does **not** produce EOF on `output` (the pseudoconsole
    /// keeps the pipe open), so `output` disconnecting is a Unix-only signal
    /// — this channel is the active, cross-platform one. See
    /// `PortablePtyHost::open`'s exit-waiter thread and, in the `banto`
    /// crate, `embedded::session::PtyHandle::poll`'s doc for how the two
    /// combine.
    pub exited: Receiver<()>,
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

/// Spawns a child inside a PTY.
pub trait PtyHost {
    fn open(&self, argv: &[String], cwd: Option<&Path>, rows: u16, cols: u16) -> Result<PtyIo>;
}

/// [`PtyHost`] backed by `portable-pty` (ConPTY on Windows, a Unix pty
/// elsewhere).
#[derive(Debug, Default, Clone, Copy)]
pub struct PortablePtyHost;

impl PtyHost for PortablePtyHost {
    fn open(&self, argv: &[String], cwd: Option<&Path>, rows: u16, cols: u16) -> Result<PtyIo> {
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
        let (exited_tx, exited_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = exited_tx.send(());
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
        pub exit_sender: Arc<Mutex<Option<mpsc::Sender<()>>>>,
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
    }

    impl MockPtyHost {
        /// Simulate the child exiting: fires the `exited` channel of the
        /// most recently opened `PtyIo`. A no-op if nothing is open, or the
        /// exit was already fired (a real exit only ever happens once).
        pub fn fire_exit(&self) {
            if let Some(tx) = self.exit_sender.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
    }

    impl PtyHost for MockPtyHost {
        fn open(
            &self,
            _argv: &[String],
            _cwd: Option<&Path>,
            _rows: u16,
            _cols: u16,
        ) -> Result<PtyIo> {
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
    struct MockKiller(Arc<Mutex<u32>>, Arc<Mutex<Option<mpsc::Sender<()>>>>);

    impl Killer for MockKiller {
        fn kill(&mut self) -> Result<()> {
            *self.0.lock().unwrap() += 1;
            if let Some(tx) = self.1.lock().unwrap().take() {
                let _ = tx.send(());
            }
            Ok(())
        }
    }
}
