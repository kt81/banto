//! A single embedded child terminal, split along the sans-IO line (see
//! `docs/DISCIPLINE.md` §2): `banto_core::screen::Screen` is the `vt100`
//! model — pure state, fed by output chunks, safe to hold in the emporium's
//! core `State` — and [`PtyHandle`] is the channels to the PTY child —
//! input/resize/kill/output, all I/O, held in the shell's own registry.
//! [`EmbeddedSession`] composes both back into the single handle the
//! standalone `banto _embed` demo (`super::run_embedded`) still uses; the
//! emporium (`banto_core::engine`) uses `Screen`/`PtyHandle` directly
//! instead.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use banto_core::input::KeyEvent;
use banto_core::key_encode::key_to_bytes;
use banto_core::screen::Screen;
use banto_io::pty::{PtyHost, PtyIo, Resizer};

/// The result of one non-blocking poll of a [`PtyHandle`]'s output channel.
pub(crate) enum PtyPoll {
    /// One chunk of output — becomes `Event::PtyOutput`.
    Chunk(Vec<u8>),
    /// Nothing available right now; the child is still running.
    Empty,
    /// The channel has disconnected: the child exited and its reader thread
    /// wound down, with every chunk it ever sent already drained (this
    /// variant is only reached once no `Chunk` remains) — becomes
    /// `Event::PtyExited`.
    Disconnected,
}

/// The I/O channels to a hosted PTY child: output (drained non-blockingly),
/// input, resize, and kill. Lives in the shell's own registry, keyed by
/// `engine::SessionKey` — never touched from `update`.
pub(crate) struct PtyHandle {
    io: PtyIo,
    /// Latches once `io.exited` has fired — see [`Self::poll`]'s doc for why
    /// this can't just be "check `io.exited` every time" (a `Receiver<()>`
    /// only ever fires once, and the one-poll grace period needs to
    /// remember it already happened).
    exit_observed: bool,
}

impl PtyHandle {
    pub(crate) fn open(
        host: &dyn PtyHost,
        argv: &[String],
        cwd: Option<&Path>,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        Ok(Self {
            io: host.open(argv, cwd, rows, cols)?,
            exit_observed: false,
        })
    }

    /// Poll for the child's next output chunk, non-blocking. A single
    /// primitive (rather than a separate "is there output" / "has it
    /// exited" pair) so nothing can observe `Disconnected` by discarding a
    /// chunk that arrived in the same instant.
    ///
    /// Two independent exit signals feed this, because `io.output`
    /// disconnecting (the Unix path: the child exits, the reader thread's
    /// `read()` returns EOF, the sender drops) never fires on ConPTY — a
    /// child's exit doesn't produce EOF on the pseudoconsole's master, so
    /// `io.exited` (an active `child.wait()` in its own thread — see
    /// `PortablePtyHost::open`) is the cross-platform signal. Precedence:
    /// (a) a pending output chunk always wins, drained before declaring
    /// death, so a dying child's tail output is never dropped; (b) `output`
    /// actually disconnecting is `Disconnected` immediately (the Unix path,
    /// unchanged); (c) otherwise, once `io.exited` has fired, the *next*
    /// poll that still finds `output` empty is `Disconnected` — the first
    /// such poll only latches [`Self::exit_observed`] and reports `Empty`,
    /// a one-tick grace for the small race where `wait()` returns while the
    /// reader thread still holds a chunk it hasn't forwarded yet (the
    /// shell's ~50ms loop cadence makes one tick plenty).
    pub(crate) fn poll(&mut self) -> PtyPoll {
        match self.io.output.try_recv() {
            Ok(chunk) => return PtyPoll::Chunk(chunk),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return PtyPoll::Disconnected,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if self.exit_observed {
            return PtyPoll::Disconnected;
        }
        if self.io.exited.try_recv().is_ok() {
            self.exit_observed = true;
        }
        PtyPoll::Empty
    }

    /// The child's OS pid, when the platform reports one — how a
    /// freshly-spawned session is matched to the `sessions/<pid>.json` it
    /// writes at startup (see `PtyIo::pid`).
    pub(crate) fn pid(&self) -> Option<u32> {
        self.io.pid
    }

    /// Encode `key` and forward it to the child's stdin.
    pub(crate) fn send_key(&mut self, key: &KeyEvent) {
        let bytes = key_to_bytes(key);
        if !bytes.is_empty() {
            let _ = self.io.input.write_all(&bytes);
            let _ = self.io.input.flush();
        }
    }

    /// Forward raw bytes (mouse report, paste, relay nudge/submit) to the
    /// child's stdin — the single stdin-write path.
    pub(crate) fn send_bytes(&mut self, bytes: &[u8]) {
        let _ = self.io.input.write_all(bytes);
        let _ = self.io.input.flush();
    }

    /// Resize the child's PTY.
    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.io.resizer.resize(rows, cols);
    }

    /// Kill the child (`Cmd::KillPty`'s executor).
    pub(crate) fn kill(&mut self) -> Result<()> {
        self.io.killer.kill()
    }

    /// Begin the child's *normal* shutdown instead of killing it outright,
    /// so an embedded `claude` finalizes its session data exactly as it
    /// would on any ordinary window close: hang up its terminal, then drop
    /// the writer and the PTY master (`resizer` is `PtyIo`'s sole owner of
    /// the master — see `PortablePtyHost::open`).
    ///
    /// The explicit [`banto_io::pty::Hangup`] is what makes this work off
    /// Windows at all. Dropping banto's own two copies of the master looks
    /// like a hangup and is one on ConPTY, but on Unix the tty only hangs up
    /// when the *last* fd closes — and the reader thread is parked in a
    /// `read()` holding a third, which that very hangup was supposed to
    /// release. Left to the drops alone the child never hears anything, sits
    /// out the whole shutdown grace, and is force-killed (measured on WSL:
    /// 5s and a `SIGKILL`, versus ~0.5s and a clean exit once the hangup is
    /// sent).
    ///
    /// Skipped for a child already known to have exited: its pid may have
    /// been recycled by now, and signalling a stranger's process group is
    /// not a mistake worth risking to save a no-op.
    ///
    /// Does not kill and does not wait — pair with [`Self::has_exited`] and a
    /// deadline, then fall back to [`Self::kill`] for stragglers (see
    /// `emporium::shutdown_handles`, which owns that policy).
    pub(crate) fn begin_graceful_close(&mut self) {
        if !self.has_exited() {
            let _ = self.io.hangup.hangup();
        }
        self.io.input = Box::new(std::io::sink());
        self.io.resizer = Box::new(NullResizer);
    }

    /// Non-blocking: has the child actually exited? Distinct from
    /// [`Self::poll`] (which also drains output and adds a one-tick grace
    /// before reporting `Disconnected`) — shutdown only cares whether it's
    /// gone, and is never followed by another `poll()` on the same handle.
    pub(crate) fn has_exited(&mut self) -> bool {
        if !self.exit_observed && self.io.exited.try_recv().is_ok() {
            self.exit_observed = true;
        }
        self.exit_observed
    }
}

/// Swapped in by [`PtyHandle::begin_graceful_close`]: resizing a child
/// that's already being torn down is meaningless, and dropping the real
/// resizer — the sole owner of the PTY master — is what actually closes it.
struct NullResizer;

impl Resizer for NullResizer {
    fn resize(&self, _rows: u16, _cols: u16) -> Result<()> {
        Ok(())
    }
}

/// Polls `handles` until every one has exited or `deadline` passes, sleeping
/// `poll_interval` between sweeps — the shared-deadline half of graceful
/// shutdown. A free function (not a method) so one deadline spans however
/// many handles are passed in, rather than each handle getting its own.
pub(crate) fn wait_for_exit_or_deadline(
    handles: &mut [&mut PtyHandle],
    deadline: Instant,
    poll_interval: Duration,
) {
    loop {
        if handles.iter_mut().all(|handle| handle.has_exited()) {
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(poll_interval);
    }
}

/// One hosted session: a [`Screen`] plus the [`PtyHandle`] that feeds it.
/// Kept as a composed convenience type for `super::run_embedded`'s standalone
/// full-screen demo, which has no `update`/`Cmd` plumbing of its own to split
/// across; the emporium uses `Screen`/`PtyHandle` separately instead (see the
/// module doc).
pub struct EmbeddedSession {
    screen: Screen,
    handle: PtyHandle,
}

impl EmbeddedSession {
    /// Spawn `argv` via `host` and start modelling its output at `rows`x`cols`.
    pub fn open(
        host: &dyn PtyHost,
        argv: &[String],
        cwd: Option<&Path>,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        Ok(Self {
            screen: Screen::new(rows, cols),
            handle: PtyHandle::open(host, argv, cwd, rows, cols)?,
        })
    }

    /// Drain all currently-available child output into the terminal model.
    /// Returns whether anything was processed (i.e. a redraw is warranted).
    pub fn pump(&mut self) -> bool {
        let mut changed = false;
        while let PtyPoll::Chunk(bytes) = self.handle.poll() {
            self.screen.process(&bytes);
            changed = true;
        }
        changed
    }

    /// Encode `key` and forward it to the child's stdin.
    pub fn send_key(&mut self, key: &KeyEvent) {
        self.handle.send_key(key);
    }

    /// Resize the child's PTY and the terminal model (no-op if unchanged).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.screen.resize(rows, cols) {
            self.handle.resize(rows, cols);
        }
    }

    /// The current terminal screen, for rendering.
    pub fn screen(&self) -> &vt100::Screen {
        self.screen.screen()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use banto_core::input::{KeyCode, KeyEvent, Modifiers};
    use banto_io::pty::mock::MockPtyHost;

    use super::{EmbeddedSession, PtyHandle, PtyPoll, wait_for_exit_or_deadline};

    fn open(host: &MockPtyHost) -> EmbeddedSession {
        EmbeddedSession::open(host, &["child".to_string()], None, 24, 80).unwrap()
    }

    fn open_handle(host: &MockPtyHost) -> PtyHandle {
        PtyHandle::open(host, &["child".to_string()], None, 24, 80).unwrap()
    }

    #[test]
    fn pump_reflects_child_output() {
        let host = MockPtyHost {
            script: b"hi".to_vec(),
            ..Default::default()
        };
        let mut session = open(&host);
        assert!(session.pump());
        assert_eq!(session.screen().cell(0, 0).unwrap().contents(), "h");
        assert_eq!(session.screen().cell(0, 1).unwrap().contents(), "i");
        assert!(!session.pump()); // nothing left to drain
    }

    #[test]
    fn send_key_writes_encoded_bytes() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let host = MockPtyHost {
            captured: captured.clone(),
            ..Default::default()
        };
        let mut session = open(&host);
        session.send_key(&KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        session.send_key(&KeyEvent::new(KeyCode::Char('a'), Modifiers::NONE));
        assert_eq!(&*captured.lock().unwrap(), b"\ra");
    }

    #[test]
    fn resize_updates_model_and_forwards_once() {
        let resizes = Arc::new(Mutex::new(Vec::new()));
        let host = MockPtyHost {
            resizes: resizes.clone(),
            ..Default::default()
        };
        let mut session = open(&host);
        session.resize(10, 40);
        assert_eq!(session.screen().size(), (10, 40));
        session.resize(10, 40); // unchanged -> no extra resize
        assert_eq!(&*resizes.lock().unwrap(), &[(10, 40)]);
    }

    // --- PtyHandle::poll: ConPTY active-exit detection ----------------------

    #[test]
    fn poll_reports_chunks_then_stays_empty_with_no_exit() {
        let host = MockPtyHost {
            script: b"hi".to_vec(),
            ..Default::default()
        };
        let mut handle = open_handle(&host);
        assert!(matches!(handle.poll(), PtyPoll::Chunk(bytes) if bytes == b"hi"));
        assert!(matches!(handle.poll(), PtyPoll::Empty));
        assert!(
            matches!(handle.poll(), PtyPoll::Empty),
            "stays empty, never on its own"
        );
    }

    #[test]
    fn poll_drains_a_queued_chunk_before_exit_then_grace_then_disconnected() {
        let host = MockPtyHost {
            script: b"hi".to_vec(),
            ..Default::default()
        };
        let mut handle = open_handle(&host);
        host.fire_exit();
        // A pending chunk always wins, even though the exit already fired.
        assert!(matches!(handle.poll(), PtyPoll::Chunk(bytes) if bytes == b"hi"));
        // One-poll grace: exit observed, reported Empty once.
        assert!(matches!(handle.poll(), PtyPoll::Empty));
        // Now really gone.
        assert!(matches!(handle.poll(), PtyPoll::Disconnected));
    }

    #[test]
    fn poll_grace_then_disconnected_when_already_empty() {
        let host = MockPtyHost::default();
        let mut handle = open_handle(&host);
        host.fire_exit();
        assert!(matches!(handle.poll(), PtyPoll::Empty), "one-tick grace");
        assert!(matches!(handle.poll(), PtyPoll::Disconnected));
    }

    #[test]
    fn poll_is_disconnected_via_the_unix_path_with_no_exit_signal_at_all() {
        let host = MockPtyHost {
            unix_style_exit: true,
            ..Default::default()
        };
        let mut handle = open_handle(&host);
        // `exited` never fires; `output` disconnecting on its own (the real
        // Unix behavior) is enough on its own, with no grace tick.
        assert!(matches!(handle.poll(), PtyPoll::Disconnected));
    }

    #[test]
    fn poll_reaches_disconnected_after_kill() {
        let host = MockPtyHost::default();
        let mut handle = open_handle(&host);
        handle.kill().unwrap();
        assert!(matches!(handle.poll(), PtyPoll::Empty), "one-tick grace");
        assert!(matches!(handle.poll(), PtyPoll::Disconnected));
    }

    // --- graceful shutdown: begin_graceful_close / has_exited / wait ------

    #[test]
    fn has_exited_is_false_until_the_exit_signal_actually_fires() {
        let host = MockPtyHost::default();
        let mut handle = open_handle(&host);
        assert!(!handle.has_exited());
        host.fire_exit();
        assert!(handle.has_exited());
        // Latches: still true on a second check, not just the one that saw it.
        assert!(handle.has_exited());
    }

    #[test]
    fn begin_graceful_close_hangs_the_child_up_rather_than_trusting_the_dropped_master() {
        // The regression this pins is invisible on Windows and total on
        // Unix: with only the drops, the reader thread's clone of the master
        // keeps the tty from hanging up, the child never learns its terminal
        // is gone, and the shutdown sweep force-kills it a full grace period
        // later.
        let hangups = Arc::new(Mutex::new(0));
        let kills = Arc::new(Mutex::new(0));
        let host = MockPtyHost {
            hangups: hangups.clone(),
            kills: kills.clone(),
            ..Default::default()
        };
        let mut handle = open_handle(&host);

        handle.begin_graceful_close();

        assert_eq!(*hangups.lock().unwrap(), 1);
        assert_eq!(
            *kills.lock().unwrap(),
            0,
            "a hangup is a request, not a kill"
        );
    }

    #[test]
    fn begin_graceful_close_never_signals_a_child_that_has_already_exited() {
        // Its pid is fair game for reuse the moment it is reaped; a hangup
        // aimed at whoever holds it now would be someone else's problem.
        let hangups = Arc::new(Mutex::new(0));
        let host = MockPtyHost {
            hangups: hangups.clone(),
            ..Default::default()
        };
        let mut handle = open_handle(&host);
        host.fire_exit();

        handle.begin_graceful_close();

        assert_eq!(*hangups.lock().unwrap(), 0);
    }

    #[test]
    fn begin_graceful_close_stops_forwarding_writes_and_resizes_without_killing() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let resizes = Arc::new(Mutex::new(Vec::new()));
        let kills = Arc::new(Mutex::new(0));
        let host = MockPtyHost {
            captured: captured.clone(),
            resizes: resizes.clone(),
            kills: kills.clone(),
            ..Default::default()
        };
        let mut handle = open_handle(&host);
        handle.begin_graceful_close();
        handle.send_bytes(b"still typing?");
        handle.resize(10, 40);
        assert!(captured.lock().unwrap().is_empty(), "writer was dropped");
        assert!(
            resizes.lock().unwrap().is_empty(),
            "master/resizer was dropped"
        );
        assert_eq!(*kills.lock().unwrap(), 0, "a graceful close is not a kill");
    }

    #[test]
    fn wait_for_exit_or_deadline_returns_promptly_once_every_handle_has_exited() {
        let host = MockPtyHost::default();
        let mut handle = open_handle(&host);
        host.fire_exit();
        let start = Instant::now();
        wait_for_exit_or_deadline(
            &mut [&mut handle],
            start + Duration::from_secs(5),
            Duration::from_millis(50),
        );
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "an already-exited handle should not wait out the deadline"
        );
    }

    #[test]
    fn wait_for_exit_or_deadline_gives_up_once_the_deadline_passes() {
        let host = MockPtyHost::default();
        let mut handle = open_handle(&host);
        // Never fired: this handle never exits.
        let deadline = Instant::now() + Duration::from_millis(30);
        wait_for_exit_or_deadline(&mut [&mut handle], deadline, Duration::from_millis(5));
        assert!(Instant::now() >= deadline);
        assert!(!handle.has_exited());
    }

    #[test]
    fn wait_for_exit_or_deadline_keeps_waiting_while_any_handle_is_still_alive() {
        let host_a = MockPtyHost::default();
        let host_b = MockPtyHost::default();
        let mut a = open_handle(&host_a);
        let mut b = open_handle(&host_b);
        host_a.fire_exit(); // a exits immediately; b never does.
        let deadline = Instant::now() + Duration::from_millis(30);
        wait_for_exit_or_deadline(&mut [&mut a, &mut b], deadline, Duration::from_millis(5));
        assert!(a.has_exited());
        assert!(!b.has_exited());
    }
}
