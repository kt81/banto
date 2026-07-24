//! A single embedded child terminal, split along the sans-IO line (see
//! `docs/DISCIPLINE.md` §2): [`Screen`] is the `vt100` model — pure state,
//! fed by output chunks, safe to hold in the emporium's core `State` — and
//! [`PtyHandle`] is the channels to the PTY child — input/resize/kill/output,
//! all I/O, held in the shell's own registry. [`EmbeddedSession`] composes
//! both back into the single handle the standalone `banto _embed` demo
//! (`super::run_embedded`) still uses; the emporium (`super::engine`) uses
//! `Screen`/`PtyHandle` directly instead.

use std::io::Write;
use std::path::Path;

use anyhow::Result;
use banto_core::input::KeyEvent;

use super::input::key_to_bytes;
use super::pty::{PtyHost, PtyIo};

/// The `vt100` terminal model for one hosted session: pure state, driven by
/// output chunks ([`Self::process`]) and resized in step with its `PtyHandle`
/// ([`Self::resize`] reports whether the size actually changed, so the
/// caller knows whether to also resize the PTY itself — an I/O action this
/// type never performs).
pub(crate) struct Screen {
    parser: vt100::Parser,
    size: (u16, u16),
}

impl Screen {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            size: (rows, cols),
        }
    }

    /// Feed one chunk of the child's output into the model.
    pub(crate) fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize the model. Returns whether the size actually changed (a no-op
    /// resize is not worth forwarding to the PTY).
    pub(crate) fn resize(&mut self, rows: u16, cols: u16) -> bool {
        if self.size == (rows, cols) {
            return false;
        }
        self.parser.screen_mut().set_size(rows, cols);
        self.size = (rows, cols);
        true
    }

    /// The current terminal screen, for rendering.
    pub(crate) fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}

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

    use banto_core::input::{KeyCode, KeyEvent, Modifiers};

    use super::super::pty::mock::MockPtyHost;
    use super::{EmbeddedSession, PtyHandle, PtyPoll};

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
}
