//! The emporium's per-pane `vt100` terminal model — pure state, no I/O (see
//! `docs/DISCIPLINE.md` §2). Fed by output chunks the shell reads from a PTY
//! (`banto_io::pty` / `banto::embedded::session::PtyHandle`), which is what
//! actually touches a process; this only interprets the bytes it's handed.

/// The `vt100` terminal model for one hosted session: pure state, driven by
/// output chunks ([`Self::process`]) and resized in step with its PTY handle
/// ([`Self::resize`] reports whether the size actually changed, so the
/// caller knows whether to also resize the PTY itself — an I/O action this
/// type never performs).
pub struct Screen {
    parser: vt100::Parser,
    size: (u16, u16),
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            size: (rows, cols),
        }
    }

    /// Feed one chunk of the child's output into the model.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize the model. Returns whether the size actually changed (a no-op
    /// resize is not worth forwarding to the PTY).
    pub fn resize(&mut self, rows: u16, cols: u16) -> bool {
        if self.size == (rows, cols) {
            return false;
        }
        self.parser.screen_mut().set_size(rows, cols);
        self.size = (rows, cols);
        true
    }

    /// The current terminal screen, for rendering.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}
