//! A single embedded child terminal: a `vt100` model driven by the child's
//! output, plus input/resize forwarding. UI-free and unit-testable via a mock
//! [`PtyHost`](super::pty::PtyHost).

use std::io::Write;
use std::path::Path;

use anyhow::Result;
use crossterm::event::KeyEvent;

use super::input::key_to_bytes;
use super::pty::{PtyHost, PtyIo};

/// One hosted session: its terminal model (`vt100`) and the channels to its
/// PTY child.
pub struct EmbeddedSession {
    parser: vt100::Parser,
    io: PtyIo,
    size: (u16, u16),
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
        let io = host.open(argv, cwd, rows, cols)?;
        Ok(Self {
            parser: vt100::Parser::new(rows, cols, 0),
            io,
            size: (rows, cols),
        })
    }

    /// Drain all currently-available child output into the terminal model.
    /// Returns whether anything was processed (i.e. a redraw is warranted).
    pub fn pump(&mut self) -> bool {
        let mut changed = false;
        while let Ok(bytes) = self.io.output.try_recv() {
            self.parser.process(&bytes);
            changed = true;
        }
        changed
    }

    /// Encode `key` and forward it to the child's stdin.
    pub fn send_key(&mut self, key: &KeyEvent) {
        let bytes = key_to_bytes(key);
        if !bytes.is_empty() {
            let _ = self.io.input.write_all(&bytes);
            let _ = self.io.input.flush();
        }
    }

    /// Resize the child's PTY and the terminal model (no-op if unchanged).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.size != (rows, cols) {
            let _ = self.io.resizer.resize(rows, cols);
            self.parser.screen_mut().set_size(rows, cols);
            self.size = (rows, cols);
        }
    }

    /// The current terminal screen, for rendering.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::pty::mock::MockPtyHost;
    use super::EmbeddedSession;

    fn open(host: &MockPtyHost) -> EmbeddedSession {
        EmbeddedSession::open(host, &["child".to_string()], None, 24, 80).unwrap()
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
        session.send_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        session.send_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
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
}
