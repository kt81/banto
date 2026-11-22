//! The quiet time after an interrupting Esc: every other write to the pane waits
//! for it, then goes out in the order it came.
//!
//! The child reads a lone ESC as the Esc key only when nothing follows it for a
//! moment. Measured on Claude 2.1.330 on Windows (2026-11-22): a byte that arrives
//! 50 ms or less after the ESC merges with it as an Alt chord, so the turn is not
//! stopped and the byte is gone; 52 ms or more is read as an Esc and then the byte.
//! Operator keys, a paste, a mouse or focus report and banto's own nudge all take
//! the same one write path into the pane, and any of them could land inside that
//! window, so the path itself holds them back instead of every producer being
//! taught about it.

use std::collections::VecDeque;
use std::io::Write;
use std::time::{Duration, Instant};

/// How long after an interrupting Esc nothing else is written. The measured cutoff
/// is about 50 ms (see the module doc); 300 ms is a margin over it that a scheduler
/// stall or a slow ConPTY hop cannot eat, and short enough that a held keystroke is
/// not felt as lost. It is not itself measured, and the pace of the event loop
/// (10 to 50 ms) can only lengthen it.
pub(crate) const QUIET_AFTER_INTERRUPT: Duration = Duration::from_millis(300);

const ESC: &[u8] = b"\x1b";

enum Held {
    /// One write exactly as it was asked for: never merged with its neighbours,
    /// because where a chunk ends is something a child's input parser can see.
    Bytes(Vec<u8>),
    /// An interrupt that came while the quiet time of an earlier one still ran.
    Interrupt,
}

/// The writes behind one pane's quiet time. Generic over the writer and given the
/// clock, so the spacing is testable without a child or a sleep.
#[derive(Default)]
pub(crate) struct QuietWrites {
    quiet_until: Option<Instant>,
    held: VecDeque<Held>,
}

impl QuietWrites {
    fn is_quiet(&self, now: Instant) -> bool {
        self.quiet_until.is_some_and(|until| now < until)
    }

    /// Write one Esc now and start the quiet time. The one write that is never held:
    /// stopping the turn is what the caller is paying for. The exception is an
    /// interrupt that arrives inside the quiet time of an earlier one, which is held
    /// like anything else: written at once it would sit inside the earlier Esc's
    /// window and merge with it. The tick writes one Esc per pane per second, so in
    /// practice this does not happen.
    pub(crate) fn interrupt<W: Write + ?Sized>(&mut self, out: &mut W, now: Instant) {
        if self.is_quiet(now) {
            self.held.push_back(Held::Interrupt);
            return;
        }
        write_one(out, ESC);
        self.quiet_until = Some(now + QUIET_AFTER_INTERRUPT);
    }

    /// Write `bytes` now, or after the quiet time if one is running. Anything
    /// already held keeps its place ahead of it.
    pub(crate) fn write<W: Write + ?Sized>(&mut self, out: &mut W, bytes: &[u8], now: Instant) {
        self.flush_due(out, now);
        if self.is_quiet(now) || !self.held.is_empty() {
            self.held.push_back(Held::Bytes(bytes.to_vec()));
        } else {
            write_one(out, bytes);
        }
    }

    /// Let go of what the quiet time held, once it is over. Called on every turn
    /// of the event loop, so a held keystroke leaves within one poll of the time.
    /// An interrupt that was held starts its own quiet time when it goes out, and
    /// what came after it keeps waiting.
    pub(crate) fn flush_due<W: Write + ?Sized>(&mut self, out: &mut W, now: Instant) {
        if self.is_quiet(now) {
            return;
        }
        self.quiet_until = None;
        while let Some(held) = self.held.pop_front() {
            match held {
                Held::Bytes(bytes) => write_one(out, &bytes),
                Held::Interrupt => {
                    write_one(out, ESC);
                    self.quiet_until = Some(now + QUIET_AFTER_INTERRUPT);
                    return;
                }
            }
        }
    }

    /// Drop everything held: the pane is ending, and what was asked of it goes
    /// with it.
    pub(crate) fn discard(&mut self) {
        self.held.clear();
        self.quiet_until = None;
    }

    /// Whether anything is still waiting, for the loop to know it has work.
    #[cfg(test)]
    pub(crate) fn holds_anything(&self) -> bool {
        !self.held.is_empty()
    }
}

fn write_one<W: Write + ?Sized>(out: &mut W, bytes: &[u8]) {
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A writer that records each write call and each flush, in order.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<Vec<u8>>>>);

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Recorder {
        fn writes(&self) -> Vec<Vec<u8>> {
            self.0.lock().unwrap().clone()
        }
    }

    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn clock() -> Instant {
        Instant::now()
    }

    #[test]
    fn writes_pass_straight_through_when_no_interrupt_was_sent() {
        let (mut out, mut quiet, t) = (Recorder::default(), QuietWrites::default(), clock());
        quiet.write(&mut out, b"a", t);
        quiet.write(&mut out, b"b", t);
        assert_eq!(out.writes(), [b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn the_esc_itself_goes_out_at_once() {
        let (mut out, mut quiet, t) = (Recorder::default(), QuietWrites::default(), clock());
        quiet.interrupt(&mut out, t);
        assert_eq!(out.writes(), [b"\x1b".to_vec()]);
    }

    #[test]
    fn what_comes_inside_the_quiet_time_is_held_and_flushed_in_order_after_it() {
        let (mut out, mut quiet, t) = (Recorder::default(), QuietWrites::default(), clock());
        quiet.interrupt(&mut out, t);
        quiet.write(&mut out, b"key", t + MS(10));
        quiet.write(&mut out, b"\x1b[200~paste\x1b[201~", t + MS(60));
        quiet.write(&mut out, b"\x1b[I", t + MS(299));
        assert_eq!(
            out.writes(),
            [b"\x1b".to_vec()],
            "nothing inside the 300 ms"
        );

        quiet.flush_due(&mut out, t + MS(299));
        assert_eq!(out.writes().len(), 1);
        quiet.flush_due(&mut out, t + MS(300));
        assert_eq!(
            out.writes(),
            [
                b"\x1b".to_vec(),
                b"key".to_vec(),
                b"\x1b[200~paste\x1b[201~".to_vec(),
                b"\x1b[I".to_vec()
            ],
            "in order, each as the write it was"
        );
        assert!(!quiet.holds_anything());
    }

    #[test]
    fn a_write_after_the_quiet_time_goes_out_directly_with_nothing_held_before_it() {
        let (mut out, mut quiet, t) = (Recorder::default(), QuietWrites::default(), clock());
        quiet.interrupt(&mut out, t);
        quiet.write(&mut out, b"early", t + MS(100));
        // The loop did not get to flush first: the write itself releases what was
        // held, ahead of itself.
        quiet.write(&mut out, b"late", t + MS(400));
        assert_eq!(
            out.writes(),
            [b"\x1b".to_vec(), b"early".to_vec(), b"late".to_vec()]
        );
    }

    #[test]
    fn a_second_interrupt_inside_the_quiet_time_is_held_so_the_two_never_merge() {
        let (mut out, mut quiet, t) = (Recorder::default(), QuietWrites::default(), clock());
        quiet.interrupt(&mut out, t);
        quiet.interrupt(&mut out, t + MS(20));
        quiet.write(&mut out, b"after", t + MS(30));
        assert_eq!(out.writes(), [b"\x1b".to_vec()]);

        quiet.flush_due(&mut out, t + MS(300));
        // The held Esc starts its own quiet time: what followed it waits again.
        assert_eq!(out.writes(), [b"\x1b".to_vec(), b"\x1b".to_vec()]);
        quiet.flush_due(&mut out, t + MS(599));
        assert_eq!(out.writes().len(), 2);
        quiet.flush_due(&mut out, t + MS(600));
        assert_eq!(
            out.writes(),
            [b"\x1b".to_vec(), b"\x1b".to_vec(), b"after".to_vec()]
        );
    }

    #[test]
    fn nothing_is_dropped_or_reordered_across_several_interrupts() {
        let (mut out, mut quiet, t) = (Recorder::default(), QuietWrites::default(), clock());
        quiet.interrupt(&mut out, t);
        quiet.write(&mut out, b"1", t + MS(1));
        quiet.interrupt(&mut out, t + MS(2));
        quiet.write(&mut out, b"2", t + MS(3));
        let mut at = t;
        for _ in 0..40 {
            at += MS(50);
            quiet.flush_due(&mut out, at);
        }
        assert_eq!(
            out.writes(),
            [
                b"\x1b".to_vec(),
                b"1".to_vec(),
                b"\x1b".to_vec(),
                b"2".to_vec()
            ]
        );
    }

    #[test]
    fn two_esc_writes_are_never_closer_than_the_quiet_time() {
        // The writer stamps each write with the clock the queue was given.
        let mut quiet = QuietWrites::default();
        let t = clock();
        let stamps = Arc::new(Mutex::new(Vec::new()));
        type Stamps = Vec<(Vec<u8>, Instant)>;
        struct Stamping(Arc<Mutex<Stamps>>, Arc<Mutex<Instant>>);
        impl Write for Stamping {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let now = *self.1.lock().unwrap();
                self.0.lock().unwrap().push((buf.to_vec(), now));
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let now = Arc::new(Mutex::new(t));
        let mut out = Stamping(stamps.clone(), now.clone());
        for step in 0..60u64 {
            let at = t + MS(step * 7);
            *now.lock().unwrap() = at;
            if step % 5 == 0 {
                quiet.interrupt(&mut out, at);
            }
            quiet.flush_due(&mut out, at);
        }
        let stamps = stamps.lock().unwrap();
        let escs: Vec<Instant> = stamps
            .iter()
            .filter(|(bytes, _)| bytes == ESC)
            .map(|(_, at)| *at)
            .collect();
        assert!(escs.len() > 1);
        for pair in escs.windows(2) {
            assert!(pair[1] - pair[0] >= QUIET_AFTER_INTERRUPT, "{pair:?}");
        }
    }

    #[test]
    fn discarding_drops_what_is_held_and_ends_the_quiet_time() {
        let (mut out, mut quiet, t) = (Recorder::default(), QuietWrites::default(), clock());
        quiet.interrupt(&mut out, t);
        quiet.write(&mut out, b"held", t + MS(5));
        quiet.discard();
        quiet.flush_due(&mut out, t + MS(1000));
        assert_eq!(out.writes(), [b"\x1b".to_vec()]);
        quiet.write(&mut out, b"fresh", t + MS(1001));
        assert_eq!(out.writes().last().unwrap(), b"fresh");
    }
}
