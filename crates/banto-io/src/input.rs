//! Raw terminal input, read off a dedicated thread and timestamped there —
//! not by whoever gets around to consuming it. Behind a trait so tests never
//! depend on a real terminal (AGENTS.md: every external process/IO
//! invocation sits behind a mockable abstraction).
//!
//! Built for `crates/banto/src/embedded/emporium.rs`'s event loop, which
//! also pumps every PTY, runs discovery/reload polls, and draws a frame —
//! all on the same thread it used to call `crossterm::event::read()` from.
//! A single-threaded `poll`-then-`read` stamps an event with "when the loop
//! got back around to it," not "when it arrived" — the two only diverge
//! when the loop is busy elsewhere, which is exactly when the difference
//! matters (a `PasteAccumulator` gap measured across a stall this thread
//! caused, not one the OS delivered). Moving the read to its own thread and
//! stamping immediately on return removes that conflation at the source.

use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

/// One raw terminal event, stamped the instant it was read off the OS.
#[derive(Debug)]
pub struct StampedEvent {
    pub event: crossterm::event::Event,
    pub at: Instant,
}

/// Reads raw terminal events on a dedicated thread and delivers them
/// through a channel, timestamped at the moment each was actually read —
/// not when the consumer's own per-iteration work allows it to look.
pub trait InputReader {
    /// Spawns (or simulates) the reader, returning the receiving end. The
    /// caller owns the channel for the life of one run; dropping the
    /// `Receiver` is how a real reader's thread is told to stop (see
    /// [`CrosstermInputReader::open`]'s doc).
    fn open(&self) -> Receiver<StampedEvent>;
}

/// A blocking source of one raw terminal event — the seam a future input
/// engine would occupy. banto may later replace crossterm as its Windows
/// input source entirely (the console has a VT-input mode where a paste or
/// drag-drop arrives with explicit begin/end markers instead of a timing
/// guess); several candidate engines are being measured against a captured
/// corpus elsewhere, and nothing is chosen yet. Whichever wins, the reader
/// thread, the channel, the stamping, and every downstream use of the stamp
/// ([`super::PasteAccumulator`] and its callers) stay exactly as they are —
/// only this one blocking call would change, as a second `impl` of this
/// trait, not a rewrite of [`spawn_reader`]. [`CrosstermEventSource`] is the
/// only implementation today; there is nothing to switch to yet, so nothing
/// else here is built for a second one in advance.
trait EventSource: Send {
    fn read(&mut self) -> std::io::Result<crossterm::event::Event>;
}

/// [`EventSource`] backed by a real, blocking `crossterm::event::read()`.
struct CrosstermEventSource;

impl EventSource for CrosstermEventSource {
    fn read(&mut self) -> std::io::Result<crossterm::event::Event> {
        crossterm::event::read()
    }
}

/// Spawns a thread that pulls one event at a time from `source`, stamping
/// each the instant it's read and sending it down the returned channel —
/// the part every [`InputReader`] shares regardless of which [`EventSource`]
/// backs it: block, stamp, send, repeat.
///
/// Shutdown: this thread is never joined. It is parked in `source.read()`,
/// which for [`CrosstermEventSource`] will not return until the operator's
/// next keystroke/mouse move/resize (or never, if none comes) — waiting on
/// that would make quitting hang on the operator's next input, which is
/// worse than the thread it would clean up. Dropping the `Receiver` (the
/// event loop returning) makes the *next* `tx.send` fail, at which point the
/// thread exits on its own; until then it is a harmless leak that OS process
/// teardown reclaims regardless, same shape as the PTY output reader thread
/// this mirrors (`pty.rs`'s `PortablePtyHost::open`,
/// `if tx.send(..).is_err() { break; }`).
fn spawn_reader<S: EventSource + 'static>(mut source: S) -> Receiver<StampedEvent> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(event) = source.read() {
            let at = Instant::now();
            if tx.send(StampedEvent { event, at }).is_err() {
                break;
            }
        }
    });
    rx
}

/// [`InputReader`] backed by [`CrosstermEventSource`].
#[derive(Debug, Default, Clone, Copy)]
pub struct CrosstermInputReader;

impl InputReader for CrosstermInputReader {
    fn open(&self) -> Receiver<StampedEvent> {
        spawn_reader(CrosstermEventSource)
    }
}

/// Not `#[cfg(test)]`: the `banto` crate's `embedded::emporium` (a
/// *different* crate) needs this for its own tests, and `#[cfg(test)]` is
/// crate-local in Rust — it never survives across a crate boundary, even
/// into a downstream crate's own test build (see `pty::mock`'s doc, the
/// same precedent). Always compiled instead (harmless: nothing in a real
/// build path ever references it, so it never reaches the linked binary).
pub mod mock {
    use super::*;
    use std::sync::Mutex;

    /// An [`InputReader`] that spawns no thread: `open` sends a pre-built
    /// script of [`StampedEvent`]s down the channel synchronously, in
    /// order, before returning it — so a test controls exactly which events
    /// exist and what each is stamped with (typically `Instant::now() +
    /// Duration::from_millis(n)`, the same pattern `paste_accum`'s own
    /// tests already use), with no real thread and no real timing.
    ///
    /// Only one `open` call's script is meaningful per instance — matching
    /// every `Deps` call site, which opens its reader exactly once per
    /// `event_loop` run.
    #[derive(Default)]
    pub struct MockInputReader {
        pub script: Mutex<Vec<StampedEvent>>,
    }

    impl MockInputReader {
        pub fn new(script: Vec<StampedEvent>) -> Self {
            Self {
                script: Mutex::new(script),
            }
        }
    }

    impl InputReader for MockInputReader {
        fn open(&self) -> Receiver<StampedEvent> {
            let (tx, rx) = mpsc::channel();
            for stamped in self.script.lock().unwrap().drain(..) {
                // A closed receiver (the test dropped it) is not a bug the
                // mock needs to report — it just means nothing is left to
                // read, the same as any real reader after shutdown.
                let _ = tx.send(stamped);
            }
            rx
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn delivers_the_script_in_order_with_its_own_stamps() {
            let now = Instant::now();
            let reader = MockInputReader::new(vec![
                StampedEvent {
                    event: crossterm::event::Event::FocusGained,
                    at: now,
                },
                StampedEvent {
                    event: crossterm::event::Event::FocusLost,
                    at: now + std::time::Duration::from_millis(2),
                },
            ]);

            let rx = reader.open();
            let first = rx.try_recv().expect("first event queued");
            assert_eq!(first.event, crossterm::event::Event::FocusGained);
            assert_eq!(first.at, now);
            let second = rx.try_recv().expect("second event queued");
            assert_eq!(second.event, crossterm::event::Event::FocusLost);
            assert_eq!(second.at, now + std::time::Duration::from_millis(2));
            assert!(rx.try_recv().is_err(), "script exhausted");
        }

        #[test]
        fn an_empty_script_yields_a_channel_with_nothing_queued() {
            let reader = MockInputReader::default();
            let rx = reader.open();
            assert!(rx.try_recv().is_err());
        }
    }
}
