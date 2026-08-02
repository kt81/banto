//! The emporium's per-pane `vt100` terminal model — pure state, no I/O (see
//! `docs/DISCIPLINE.md` §2). Fed by output chunks the shell reads from a PTY
//! (`banto_io::pty` / `banto::embedded::session::PtyHandle`), which is what
//! actually touches a process; this only interprets the bytes it's handed.

/// Lines of scrollback kept per pane, beyond the live screen — how far back
/// [`Screen::scroll`] can go. 2000: tmux's own long-standing default, a
/// number anyone coming from a terminal multiplexer already has an
/// intuition for. `vt100::Cell` is statically asserted at 32 bytes
/// (vt100-0.16.2 `src/cell.rs`: `assert!(std::mem::size_of::<Cell>() ==
/// 32)`), and a scrollback row is a `Vec<Cell>` with negligible extra
/// overhead, so at 200 columns (four brigade panes side by side is a
/// realistic width split) this is `2000 * 200 * 32 = 12,800,000` bytes
/// (~12.2 MiB) per pane, ~49 MiB for four — comfortable for a resident
/// background process to hold indefinitely.
const SCROLLBACK_LEN: usize = 2000;

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
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_LEN),
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

    /// How many lines back into scrollback the view currently sits — `0` is
    /// the live bottom. Reads back exactly what [`Self::scroll`] last
    /// clamped it to (see that method's own doc); exposed so a renderer can
    /// tell a "reading history" pane from a live one (a blinking cursor
    /// drawn at [`vt100::Screen::cursor_position`] while scrolled back would
    /// sit on top of unrelated historical text — that position is always
    /// the *live* cursor, never adjusted for scrollback).
    pub fn scrollback(&self) -> usize {
        self.screen().scrollback()
    }

    /// Move the view `delta` lines further back into scrollback (negative
    /// moves toward the live bottom). Clamped at both ends by `vt100`
    /// itself: `Screen::set_scrollback` clamps upward to how much history
    /// actually exists, and [`usize::saturating_add_signed`] here clamps
    /// downward at the live bottom rather than wrapping.
    ///
    /// A no-op while the child's alternate screen is active: `vt100`
    /// constructs the alternate grid with a hardcoded scrollback capacity of
    /// zero regardless of what this parser itself was built with
    /// (vt100-0.16.2 `src/screen.rs`, `enter_alternate_grid` /
    /// `alternate_grid: Grid::new(size, 0)`), so `set_scrollback` on it
    /// always clamps back to `0` no matter what's asked. That's the right
    /// behavior on its own, not a case this method needs to special-case: a
    /// full-screen app's alternate-screen content (Claude Code, Codex) has
    /// no history behind it worth scrolling into — what would be "back" is
    /// whatever was on screen before it took over, which is exactly what
    /// the alternate screen exists to hide.
    pub fn scroll(&mut self, delta: isize) {
        let target = self.screen().scrollback().saturating_add_signed(delta);
        self.parser.screen_mut().set_scrollback(target);
    }

    /// Whether this pane's child currently wants mouse events forwarded to
    /// it, in the one encoding banto speaks (SGR) — see `engine::update_mouse`'s
    /// doc for how this drives what gets forwarded vs. consumed: a child
    /// that wants mouse gets every event forwarded, including the wheel; a
    /// child that doesn't has its wheel consumed by banto itself instead,
    /// to scroll this pane's own [`Self::scroll`]. A child that enabled
    /// mouse reporting in a different encoding
    /// (`vt100::MouseProtocolEncoding::Default`/`Utf8` — legacy schemes
    /// that predate SGR, the one every modern full-screen TUI this codebase
    /// has seen asks for) is treated the same as one that never asked at
    /// all: banto has no encoder for those, and forwarding SGR bytes to a
    /// child expecting a different format would be worse than forwarding
    /// nothing — its wheel is consumed for scrollback too, same as a child
    /// with no mouse mode at all.
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

    /// Writes `n` numbered, newline-terminated lines (`"line0\r\n"`,
    /// `"line1\r\n"`, ...) — enough of them, against `screen`'s small test
    /// size, to push several off the top into scrollback.
    fn write_numbered_lines(screen: &mut Screen, n: u32) {
        for i in 0..n {
            screen.process(format!("line{i}\r\n").as_bytes());
        }
    }

    #[test]
    fn scrolling_back_reveals_a_line_that_scrolled_off_the_top() {
        let mut screen = Screen::new(4, 20);
        write_numbered_lines(&mut screen, 10);
        assert!(!screen.screen().contents().contains("line0"));

        screen.scroll(1_000_000); // clamps to however much scrollback exists
        assert!(screen.screen().contents().contains("line0"));
    }

    #[test]
    fn staying_at_the_bottom_keeps_following_new_output() {
        let mut screen = Screen::new(4, 20);
        write_numbered_lines(&mut screen, 10);
        assert_eq!(screen.scrollback(), 0);

        screen.process(b"line10\r\n");
        assert_eq!(
            screen.scrollback(),
            0,
            "a pane at the live bottom must keep following new output"
        );
    }

    #[test]
    fn scrolled_back_view_does_not_move_when_new_output_arrives() {
        // Windows-Terminal-style "don't yank" behavior: reading history must
        // not get dragged around by output the operator isn't watching.
        let mut screen = Screen::new(4, 20);
        write_numbered_lines(&mut screen, 10);
        screen.scroll(1_000_000); // clamp to the very oldest lines
        let before = screen.screen().contents();

        screen.process(b"line10\r\n"); // pushes one more old line into scrollback
        let after = screen.screen().contents();

        assert_eq!(
            before, after,
            "new output must not move what a scrolled-back pane is showing"
        );
    }

    #[test]
    fn scroll_is_clamped_at_the_live_bottom() {
        let mut screen = Screen::new(4, 20);
        screen.process(b"one line\r\n");
        screen.scroll(-5); // already at the bottom — must not underflow
        assert_eq!(screen.scrollback(), 0);
    }

    #[test]
    fn scroll_is_clamped_to_however_much_scrollback_actually_exists() {
        let mut screen = Screen::new(4, 20);
        write_numbered_lines(&mut screen, 10);

        screen.scroll(1_000_000);
        assert!(screen.screen().contents().contains("line0"));
        let clamped = screen.scrollback();
        screen.scroll(1); // one more step past the true max is still a no-op
        assert_eq!(screen.scrollback(), clamped);
    }

    #[test]
    fn scroll_is_a_no_op_while_the_child_uses_the_alternate_screen() {
        let mut screen = Screen::new(4, 20);
        write_numbered_lines(&mut screen, 10);
        screen.process(b"\x1b[?1049h"); // enter alternate screen, as a full-screen TUI would
        assert!(screen.screen().alternate_screen());

        screen.scroll(1_000_000);
        assert_eq!(
            screen.scrollback(),
            0,
            "vt100's alternate grid has zero scrollback capacity, so this always clamps to 0"
        );
    }

    #[test]
    fn resize_does_not_reset_the_scrollback_position() {
        let mut screen = Screen::new(4, 20);
        write_numbered_lines(&mut screen, 10);
        screen.scroll(3);

        screen.resize(6, 20);

        assert_eq!(
            screen.scrollback(),
            3,
            "a resize must not yank a scrolled-back pane down to the live bottom"
        );
    }
}
