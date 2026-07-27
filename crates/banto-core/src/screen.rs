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

    /// Whether this pane's child currently wants mouse events forwarded to
    /// it, in the one encoding banto speaks (SGR) — see
    /// `engine::update_mouse`'s doc for how this drives both what gets
    /// forwarded to the child and whether banto releases its own terminal's
    /// mouse capture. A child that enabled mouse reporting in a different
    /// encoding (`vt100::MouseProtocolEncoding::Default`/`Utf8` — legacy
    /// schemes that predate SGR, the one every modern full-screen TUI this
    /// codebase has seen asks for) is treated the same as one that never
    /// asked at all: banto has no encoder for those, and forwarding SGR
    /// bytes to a child expecting a different format would be worse than
    /// forwarding nothing.
    pub fn wants_sgr_mouse(&self) -> bool {
        let screen = self.screen();
        screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
            && screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wants_sgr_mouse_is_false_before_any_child_output() {
        let screen = Screen::new(24, 80);
        assert!(!screen.wants_sgr_mouse());
    }

    #[test]
    fn wants_sgr_mouse_is_true_once_the_child_enables_any_motion_mode_and_sgr_encoding() {
        let mut screen = Screen::new(24, 80);
        // `\x1b[?1003h` (any-motion mode) + `\x1b[?1006h` (SGR encoding) —
        // the pair a modern full-screen TUI enables together.
        screen.process(b"\x1b[?1003h\x1b[?1006h");
        assert!(screen.wants_sgr_mouse());
    }

    #[test]
    fn wants_sgr_mouse_is_false_when_the_encoding_is_not_sgr() {
        let mut screen = Screen::new(24, 80);
        // `\x1b[?1000h` (press-release mode) + `\x1b[?1005h` (UTF-8
        // encoding, not SGR) — a legacy child banto has no encoder for.
        screen.process(b"\x1b[?1000h\x1b[?1005h");
        assert!(!screen.wants_sgr_mouse());
    }

    #[test]
    fn wants_sgr_mouse_is_false_once_the_child_disables_mouse_mode_again() {
        let mut screen = Screen::new(24, 80);
        screen.process(b"\x1b[?1003h\x1b[?1006h");
        assert!(screen.wants_sgr_mouse());
        screen.process(b"\x1b[?1003l");
        assert!(!screen.wants_sgr_mouse());
    }
}
