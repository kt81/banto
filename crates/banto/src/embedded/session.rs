//! A single embedded child terminal, split along the sans-IO line (see
//! `docs/DISCIPLINE.md` §2): `banto_core::screen::Screen` is the `vt100`
//! model — pure state, fed by output chunks, safe to hold in the emporium's
//! core `State` — and [`PtyHandle`] is the channels to the PTY child —
//! input/resize/kill/output, all I/O, held in the shell's own registry.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use banto_core::engine::{Event, SessionKey};
use banto_core::model::PtyExitReason;
use banto_io::pty::{PtyHost, PtyIo, Resizer};

/// The result of one non-blocking poll of a [`PtyHandle`]'s output channel.
pub(crate) enum PtyPoll {
    /// One chunk of output — becomes `Event::PtyOutput`.
    Chunk(Vec<u8>),
    /// Nothing available right now: the child is running, or its end is still
    /// being confirmed (see [`PtyHandle::poll`]).
    Empty,
    /// Output ended without a confirmed child exit; emitted once.
    OutputLost,
    /// The waiter reported an exit and all available output was drained.
    Disconnected,
}

/// The event for a pane whose [`PtyPoll::Disconnected`] was just seen, given what
/// [`PtyHandle::exit_reason`] then said. The one place the two facts are told
/// apart, so the loop cannot map a bare disconnect to an exit: only a reason the
/// wait thread reported proves the process ended.
pub(crate) fn disconnect_event(key: SessionKey, reason: Option<PtyExitReason>) -> Event {
    match reason {
        Some(reason @ (PtyExitReason::Code(_) | PtyExitReason::Signal(_))) => Event::PtyExited {
            key,
            reason: Some(reason),
        },
        None | Some(PtyExitReason::Unknown) => Event::PtyOutputLost { key },
    }
}

/// The I/O channels to a hosted PTY child: output (drained non-blockingly),
/// input, resize, and kill. Lives in the shell's own registry, keyed by
/// `engine::SessionKey` — never touched from `update`.
pub(crate) struct PtyHandle {
    io: PtyIo,
    /// Read just before the host was asked to start the child, so nothing the
    /// child writes can be dated earlier than it. Moves with the handle when its
    /// pane is rekeyed, and is why a relaunch has a later one.
    spawned_at: std::time::SystemTime,
    /// A confirmed exit is shared by `poll`, `exit_reason`, and `has_exited`
    /// after any one of them consumes it. `Unknown` is not an exit.
    exit_reason: Option<PtyExitReason>,
    /// When `output` was first found disconnected without the child's exit
    /// having been reported (see [`Self::poll`]): the start of the bounded wait
    /// for the waiter's report.
    output_lost_since: Option<std::time::Instant>,
    output_loss_reported: bool,
    /// How long that wait lasts. A field so a test need not sleep.
    output_lost_grace: std::time::Duration,
}

/// How long a disconnected output stream waits for the child's own exit report
/// before it is given up as lost output. The report follows the process's end
/// by far less than this; the bound only has to be long enough that a real exit
/// is almost never mistaken for lost output, and it lives here — in the shell's
/// per-handle poll, which never blocks (it answers `Empty` meanwhile).
const OUTPUT_LOST_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

impl PtyHandle {
    pub(crate) fn open(
        host: &dyn PtyHost,
        argv: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
        env_remove: &[&str],
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let spawned_at = std::time::SystemTime::now();
        Ok(Self {
            io: host.open(argv, cwd, env, env_remove, rows, cols)?,
            spawned_at,
            exit_reason: None,
            output_lost_since: None,
            output_loss_reported: false,
            output_lost_grace: OUTPUT_LOST_GRACE,
        })
    }

    /// Poll for the child's next output chunk, non-blocking. A single
    /// primitive (rather than a separate "is there output" / "has it
    /// exited" pair) so nothing can observe `Disconnected` by discarding a
    /// chunk that arrived in the same instant.
    ///
    /// Two independent signals feed this, and only one of them proves an exit.
    /// `io.output` disconnecting means the reader thread stopped — on Unix that
    /// is the child's EOF, but the thread drops its sender on *any* `read()`
    /// error, and on ConPTY a child's exit produces no EOF at all. `io.exited`
    /// (an active `child.wait()` in its own thread — see
    /// `PortablePtyHost::open`) is the only evidence the process ended. In
    /// precedence order: (a) a pending chunk always wins, so a dying child's
    /// tail output is never dropped; (b) `output` disconnected: if the waiter has
    /// reported, `Disconnected` at once; if not, `Empty` for up to
    /// [`OUTPUT_LOST_GRACE`] — the Unix EOF routinely lands just before the
    /// waiter's report — and then `OutputLost` once; (c) `output`
    /// still open but the waiter has reported: the first such poll only latches
    /// the reason and answers `Empty`, a one-tick grace for the race where
    /// `wait()` returns while the reader still holds a chunk it has not forwarded
    /// (the ~50ms loop cadence makes one tick plenty), and the next poll is
    /// `Disconnected`.
    ///
    /// After `OutputLost`, polling continues until the waiter confirms exit.
    pub(crate) fn poll(&mut self) -> PtyPoll {
        match self.io.output.try_recv() {
            Ok(chunk) => return PtyPoll::Chunk(chunk),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // The stream ended. Whether the child did is the waiter's to say:
                // ask it, and give it a bounded moment — without blocking the
                // loop — before calling the output lost.
                self.latch_exit();
                if self.exit_reason.is_some() {
                    return PtyPoll::Disconnected;
                }
                let since = *self
                    .output_lost_since
                    .get_or_insert_with(std::time::Instant::now);
                return if since.elapsed() >= self.output_lost_grace && !self.output_loss_reported {
                    self.output_loss_reported = true;
                    PtyPoll::OutputLost
                } else {
                    PtyPoll::Empty
                };
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if self.exit_reason.is_some() {
            return PtyPoll::Disconnected;
        }
        self.latch_exit();
        PtyPoll::Empty
    }

    /// When this process was spawned, on the wall clock — the clock the store's
    /// `briefed_at` is on.
    pub(crate) fn spawned_at(&self) -> std::time::SystemTime {
        self.spawned_at
    }

    pub(crate) fn awaiting_exit_after_output_loss(&self) -> bool {
        self.output_loss_reported && self.exit_reason.is_none()
    }

    #[cfg(test)]
    pub(crate) fn skip_output_loss_grace(&mut self) {
        self.output_lost_grace = Duration::ZERO;
    }

    /// The child's OS pid, when the platform reports one — how a
    /// freshly-spawned session is matched to the `sessions/<pid>.json` it
    /// writes at startup (see `PtyIo::pid`).
    pub(crate) fn pid(&self) -> Option<u32> {
        self.io.pid
    }

    /// What the wait thread reported, read right after [`Self::poll`] returned
    /// `Disconnected` to decide [`disconnect_event`]. `Some` is the only proof the
    /// process exited; `None` means no report has arrived (after `poll`'s bounded
    /// wait: the output was lost, the process not known to be gone) and is never
    /// guessed into an exit. The `try_recv` here is a harmless no-op once a reason
    /// is latched.
    pub(crate) fn exit_reason(&mut self) -> Option<PtyExitReason> {
        self.latch_exit();
        self.exit_reason.clone()
    }

    fn latch_exit(&mut self) {
        if self.exit_reason.is_some() {
            return;
        }
        for _ in 0..16 {
            match self.io.exited.try_recv() {
                Ok(PtyExitReason::Unknown) => continue,
                Ok(reason) => {
                    self.exit_reason = Some(reason);
                    break;
                }
                Err(_) => break,
            }
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
    /// [`Self::poll`] (which also drains output and waits out its grace
    /// before reporting `Disconnected`) — shutdown only cares whether the
    /// waiter has reported, and is never followed by another `poll()` on the same
    /// handle.
    pub(crate) fn has_exited(&mut self) -> bool {
        self.latch_exit();
        self.exit_reason.is_some()
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use banto_core::engine::{Event, SessionKey};
    use banto_core::model::PtyExitReason;
    use banto_io::pty::mock::MockPtyHost;

    use super::{PtyHandle, PtyPoll, disconnect_event, wait_for_exit_or_deadline};

    fn open_handle(host: &MockPtyHost) -> PtyHandle {
        PtyHandle::open(host, &["child".to_string()], None, &[], &[], 24, 80).unwrap()
    }

    #[test]
    fn open_forwards_env_remove_to_the_host_unchanged() {
        let host = MockPtyHost::default();
        let _handle = PtyHandle::open(
            &host,
            &["child".to_string()],
            None,
            &[],
            &["CLAUDE_CODE_CHILD_SESSION"],
            24,
            80,
        )
        .unwrap();
        assert_eq!(
            *host.env_removes.lock().unwrap(),
            vec![vec!["CLAUDE_CODE_CHILD_SESSION".to_string()]],
            "PtyHandle::open must pass its own env_remove argument through to the host \
             unchanged — a call site silently dropping it would be invisible without this"
        );
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

    /// A reader error: the output stream is gone (the host drops its sender at
    /// once) while the child is, as far as anything has reported, still running.
    fn reader_error_host() -> MockPtyHost {
        MockPtyHost {
            unix_style_exit: true,
            ..Default::default()
        }
    }

    #[test]
    fn output_loss_is_reported_once_then_waits_for_the_actual_exit() {
        let host = reader_error_host();
        let mut handle = open_handle(&host);
        handle.output_lost_grace = std::time::Duration::ZERO;
        assert!(matches!(handle.poll(), PtyPoll::OutputLost));
        assert!(handle.awaiting_exit_after_output_loss());
        assert!(handle.exit_reason().is_none());
        assert!(matches!(handle.poll(), PtyPoll::Empty));
        assert!(matches!(handle.poll(), PtyPoll::Empty));
        host.fire_exit_with_reason(PtyExitReason::Code(7));
        assert!(matches!(handle.poll(), PtyPoll::Disconnected));
        assert_eq!(handle.exit_reason(), Some(PtyExitReason::Code(7)));
        assert!(!handle.awaiting_exit_after_output_loss());
    }

    #[test]
    fn failed_kill_after_output_loss_does_not_claim_an_exit() {
        struct FailingKiller;
        impl banto_io::pty::Killer for FailingKiller {
            fn kill(&mut self) -> anyhow::Result<()> {
                anyhow::bail!("synthetic kill failure")
            }
        }

        let host = reader_error_host();
        let mut handle = open_handle(&host);
        handle.io.killer = Box::new(FailingKiller);
        handle.output_lost_grace = std::time::Duration::ZERO;
        assert!(matches!(handle.poll(), PtyPoll::OutputLost));
        assert!(handle.kill().is_err());
        assert!(matches!(handle.poll(), PtyPoll::Empty));
        assert!(handle.awaiting_exit_after_output_loss());
        assert_eq!(handle.exit_reason(), None);
    }

    #[test]
    fn a_reader_error_with_the_child_alive_is_output_lost_and_never_an_exit() {
        let host = reader_error_host();
        let mut handle = open_handle(&host);
        // Within the bounded wait: nothing is claimed, and the loop is not blocked.
        assert!(matches!(handle.poll(), PtyPoll::Empty));
        assert!(matches!(handle.poll(), PtyPoll::Empty));
        // The wait is over and no exit was reported: the output is lost — the
        // pane is gone, but there is NO exit reason, so the shell reports no exit.
        handle.output_lost_grace = std::time::Duration::ZERO;
        assert!(matches!(handle.poll(), PtyPoll::OutputLost));
        assert_eq!(handle.exit_reason(), None, "no report, so no exit");
    }

    #[test]
    fn an_exit_reported_during_the_wait_turns_lost_output_into_a_confirmed_exit() {
        // The Unix order: EOF first, the waiter's report a moment later.
        let host = reader_error_host();
        let mut handle = open_handle(&host);
        assert!(matches!(handle.poll(), PtyPoll::Empty));
        host.fire_exit_with_reason(PtyExitReason::Code(3));
        assert!(matches!(handle.poll(), PtyPoll::Disconnected));
        assert_eq!(handle.exit_reason(), Some(PtyExitReason::Code(3)));
    }

    #[test]
    fn an_exit_reported_before_the_output_closes_is_confirmed_at_once() {
        // The other order: the waiter first, the EOF after.
        let host = reader_error_host();
        let mut handle = open_handle(&host);
        host.fire_exit_with_reason(PtyExitReason::Signal("SIGKILL".to_string()));
        // No wait at all: the report was already there when the EOF was seen.
        assert!(matches!(handle.poll(), PtyPoll::Disconnected));
        assert_eq!(
            handle.exit_reason(),
            Some(PtyExitReason::Signal("SIGKILL".to_string()))
        );
    }

    #[test]
    fn an_exit_with_no_eof_at_all_is_still_a_confirmed_exit() {
        // The ConPTY case: the output never disconnects; only the waiter speaks.
        let host = MockPtyHost::default();
        let mut handle = open_handle(&host);
        host.fire_exit_with_reason(PtyExitReason::Code(0));
        assert!(matches!(handle.poll(), PtyPoll::Empty), "one-tick grace");
        assert!(matches!(handle.poll(), PtyPoll::Disconnected));
        assert_eq!(handle.exit_reason(), Some(PtyExitReason::Code(0)));
    }

    #[test]
    fn poll_reaches_disconnected_after_kill() {
        let host = MockPtyHost::default();
        let mut handle = open_handle(&host);
        handle.kill().unwrap();
        assert!(matches!(handle.poll(), PtyPoll::Empty), "one-tick grace");
        assert!(matches!(handle.poll(), PtyPoll::Disconnected));
    }

    // --- disconnect_event: the join between poll and core --------------------

    #[test]
    fn a_disconnect_with_a_reported_exit_is_an_exit() {
        for reason in [
            PtyExitReason::Code(0),
            PtyExitReason::Signal("SIGKILL".to_string()),
        ] {
            assert_eq!(
                disconnect_event(SessionKey::from_id("k"), Some(reason.clone())),
                Event::PtyExited {
                    key: SessionKey::from_id("k"),
                    reason: Some(reason)
                }
            );
        }
    }

    #[test]
    fn wait_failure_does_not_claim_exit_and_later_status_is_observed() {
        for reason in [
            PtyExitReason::Code(7),
            PtyExitReason::Signal("SIGTERM".into()),
        ] {
            let host = MockPtyHost::default();
            let mut handle = open_handle(&host);
            host.exit_sender
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .send(PtyExitReason::Unknown)
                .unwrap();
            assert!(!handle.has_exited());
            assert!(matches!(handle.poll(), PtyPoll::Empty));
            assert!(matches!(
                disconnect_event(SessionKey::from_id("k"), Some(PtyExitReason::Unknown)),
                Event::PtyOutputLost { .. }
            ));
            host.fire_exit_with_reason(reason.clone());
            assert_eq!(handle.exit_reason(), Some(reason));
        }
    }

    #[test]
    fn wait_failure_after_output_loss_keeps_the_handle_until_confirmed_exit() {
        let host = reader_error_host();
        let mut handle = open_handle(&host);
        handle.output_lost_grace = Duration::ZERO;
        assert!(matches!(handle.poll(), PtyPoll::OutputLost));
        host.exit_sender
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(PtyExitReason::Unknown)
            .unwrap();
        assert!(matches!(handle.poll(), PtyPoll::Empty));
        assert!(handle.awaiting_exit_after_output_loss());
        host.fire_exit_with_reason(PtyExitReason::Code(0));
        assert!(matches!(handle.poll(), PtyPoll::Disconnected));
        assert_eq!(handle.exit_reason(), Some(PtyExitReason::Code(0)));
    }

    #[test]
    fn queued_wait_failure_does_not_hide_following_exit() {
        let host = MockPtyHost::default();
        let mut handle = open_handle(&host);
        host.exit_sender
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(PtyExitReason::Unknown)
            .unwrap();
        host.fire_exit_with_reason(PtyExitReason::Code(4));
        assert_eq!(handle.exit_reason(), Some(PtyExitReason::Code(4)));
    }

    #[test]
    fn a_disconnect_with_no_reported_exit_is_lost_output_never_an_exit() {
        assert_eq!(
            disconnect_event(SessionKey::from_id("k"), None),
            Event::PtyOutputLost {
                key: SessionKey::from_id("k")
            }
        );
    }

    #[test]
    fn the_events_follow_what_poll_and_exit_reason_say_for_a_reader_error() {
        // End to end through the handle: the same disconnect becomes the event
        // its exit evidence supports.
        let host = reader_error_host();
        let mut handle = open_handle(&host);
        handle.output_lost_grace = std::time::Duration::ZERO;
        assert!(matches!(handle.poll(), PtyPoll::OutputLost));
        assert!(matches!(
            disconnect_event(SessionKey::from_id("k"), handle.exit_reason()),
            Event::PtyOutputLost { .. }
        ));

        let host = reader_error_host();
        let mut handle = open_handle(&host);
        host.fire_exit_with_reason(PtyExitReason::Code(1));
        assert!(matches!(handle.poll(), PtyPoll::Disconnected));
        assert!(matches!(
            disconnect_event(SessionKey::from_id("k"), handle.exit_reason()),
            Event::PtyExited { .. }
        ));
    }

    // --- exit_reason: what the exit report carries ----------------------------

    #[test]
    fn exit_reason_is_none_before_the_child_exits() {
        let host = MockPtyHost::default();
        let mut handle = open_handle(&host);
        assert_eq!(handle.exit_reason(), None);
    }

    #[test]
    fn exit_reason_carries_what_fire_exit_with_reason_sent() {
        let host = MockPtyHost::default();
        let mut handle = open_handle(&host);
        host.fire_exit_with_reason(PtyExitReason::Code(17));
        assert_eq!(handle.exit_reason(), Some(PtyExitReason::Code(17)));
        // Latched: still the same reason on a second read, not consumed.
        assert_eq!(handle.exit_reason(), Some(PtyExitReason::Code(17)));
    }

    #[test]
    fn exit_reason_survives_being_first_observed_through_poll_or_has_exited() {
        let host = MockPtyHost::default();
        let mut handle = open_handle(&host);
        host.fire_exit_with_reason(PtyExitReason::Signal("SIGTERM".to_string()));
        assert!(handle.has_exited());
        assert_eq!(
            handle.exit_reason(),
            Some(PtyExitReason::Signal("SIGTERM".to_string())),
            "has_exited must not swallow the reason poll/exit_reason still need"
        );
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
