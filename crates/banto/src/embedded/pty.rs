//! PTY host abstraction: spawn a child in a pseudo-terminal and expose its
//! output stream, an input sink, and a resize handle. Behind a trait so tests
//! never spawn a real process (CLAUDE.md: every external process invocation
//! sits behind a mockable abstraction).

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use anyhow::Result;
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// The four channels to a hosted child: its output chunks, an input sink, a
/// resize handle, and a kill handle. Returned by [`PtyHost::open`].
pub struct PtyIo {
    /// Chunks of the child's terminal output, pumped from a reader thread.
    pub output: Receiver<Vec<u8>>,
    /// Writes here go to the child's stdin.
    pub input: Box<dyn Write + Send>,
    /// Resizes the child's PTY (and keeps the child process alive).
    pub resizer: Box<dyn Resizer>,
    /// Kills the child. Added alongside the resize handle (touching this
    /// file once) even though nothing emits `Cmd::KillPty` yet — that's
    /// Phase 2b (session termination); the executor plumbing is deliberately
    /// ready ahead of it.
    pub killer: Box<dyn Killer>,
}

/// Resizes a hosted child's PTY.
pub trait Resizer: Send {
    fn resize(&self, rows: u16, cols: u16) -> Result<()>;
}

/// Kills a hosted child's process.
pub trait Killer: Send {
    fn kill(&mut self) -> Result<()>;
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
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        // Cloned before `child` moves into the resizer holder below: a
        // `ChildKiller` is an independent handle to the same process, so the
        // resize/kill capabilities don't need to fight over one `&mut Child`.
        let killer = child.clone_killer();

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

        Ok(PtyIo {
            output: rx,
            input,
            resizer: Box::new(PortablePtyResizer {
                master: pair.master,
                _child: child,
            }),
            killer: Box::new(PortablePtyKiller(killer)),
        })
    }
}

/// Holds the master PTY (for resizing) and the child handle (to keep the
/// process alive for the pane's lifetime).
struct PortablePtyResizer {
    master: Box<dyn MasterPty + Send>,
    _child: Box<dyn Child + Send + Sync>,
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

#[cfg(test)]
pub(crate) mod mock {
    use std::io::{self, Write};
    use std::path::Path;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;

    use super::{Killer, PtyHost, PtyIo, Resizer};

    /// A [`PtyHost`] that spawns nothing: it replays `script` as the child's
    /// output and records everything written to the child, every resize, and
    /// every kill.
    #[derive(Default)]
    pub(crate) struct MockPtyHost {
        pub script: Vec<u8>,
        pub captured: Arc<Mutex<Vec<u8>>>,
        pub resizes: Arc<Mutex<Vec<(u16, u16)>>>,
        pub kills: Arc<Mutex<u32>>,
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
            if !self.script.is_empty() {
                let _ = tx.send(self.script.clone());
            }
            Ok(PtyIo {
                output: rx,
                input: Box::new(CapturingWriter(self.captured.clone())),
                resizer: Box::new(MockResizer(self.resizes.clone())),
                killer: Box::new(MockKiller(self.kills.clone())),
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

    struct MockKiller(Arc<Mutex<u32>>);

    impl Killer for MockKiller {
        fn kill(&mut self) -> Result<()> {
            *self.0.lock().unwrap() += 1;
            Ok(())
        }
    }
}
