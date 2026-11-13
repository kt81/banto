//! The emporium's per-pane `vt100` terminal model — pure state, no I/O (see
//! `docs/DISCIPLINE.md` §2). Fed by output chunks the shell reads from a PTY
//! (`banto_io::pty` / `banto::embedded::session::PtyHandle`), which is what
//! actually touches a process; this only interprets the bytes it's handed.

use crate::program_state::{Report, ReportedState, StateReport, parse_osc};

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

/// The fewest rows a screen, and so its PTY, is ever given.
///
/// The pinned vt100 (rev 3a8186e of the psmux fork) panics when text wraps on a
/// one-row screen: `Grid::col_wrap` takes the rows it scrolled away from the row
/// it left (`grid.rs:739`), and with one row that is 0 - 1. A release build
/// wraps the subtraction instead and the `unwrap()` that follows panics
/// (`grid.rs:745`), so it is no debug-only check. A screen with no rows panics
/// at creation in a debug build. Found by a sweep of 27 classes of input (wrapping
/// text, wide glyphs, scroll sequences, region and alternate-screen changes) over
/// sizes 0 to 4 on each axis, fresh, shrunk from 24x80 and grown to it, in both
/// profiles: nothing at two rows or more and two columns or more panicked.
/// `a_screen_at_or_above_the_floor_never_panics_in_the_real_parser` re-runs it.
pub const MIN_ROWS: u16 = 2;
/// The fewest columns, for the same reason. A one-column screen is safe in a
/// release build, but a double-width glyph in it underflows `cols - width` in a
/// debug build (`screen.rs:1162`), and a screen with no columns panics as soon
/// as anything is printed; two columns hold a wide glyph.
pub const MIN_COLS: u16 = 2;

/// What a freshly spawned pane's screen and PTY start at, until the first layout
/// pass sizes them: shared so that the two cannot start out different.
pub const SPAWN_ROWS: u16 = 24;
pub const SPAWN_COLS: u16 = 80;

/// `rows` x `cols` raised to the floor: the size a screen actually takes.
fn floored(rows: u16, cols: u16) -> (u16, u16) {
    (rows.max(MIN_ROWS), cols.max(MIN_COLS))
}

/// The `vt100` terminal model for one hosted session: pure state, driven by
/// output chunks ([`Self::process`]) and resized in step with its PTY handle
/// ([`Self::resize`] reports whether the size actually changed, so the
/// caller knows whether to also resize the PTY itself — an I/O action this
/// type never performs).
pub struct Screen {
    parser: vt100::Parser<PaneModes>,
    size: (u16, u16),
}

/// The two private modes banto acts on that `vt100` does not model itself, and
/// the program's own state report. `vt100` hands anything it does not implement
/// to `unhandled_csi` / `unhandled_osc`, and distinguishes that from modes it
/// *does* implement (DECSET 2004 never arrives here), so this sees exactly the
/// sequences nobody else consumed.
///
/// Both children banto hosts drive these: Codex wraps every `Tui::draw` in
/// crossterm's `SynchronizedUpdate`, and Claude Code enables synchronized
/// output whenever `WT_SESSION` is set — which banto's PTY passes through,
/// since it inherits the environment it was launched from.
#[derive(Default)]
pub struct PaneModes {
    synchronized_update: bool,
    wants_focus_events: bool,
    /// What the program last reported about its own state (OSC 7501, see
    /// [`crate::program_state`]); `None` before any report and after `clear`.
    reported: Option<ReportedState>,
}

impl vt100::Callbacks for PaneModes {
    /// The parser calls this for every OSC it does not implement, once the
    /// sequence is complete, whichever chunks it arrived in. The only one read is
    /// the program's own state report, and only its root record: one naming a
    /// sub-record (`id=`) says nothing about the pane's own state, so it must
    /// neither replace nor clear it. A report that may have been cut short with
    /// its state lost ends what the pane reported, like `clear`.
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        match parse_osc(params) {
            Some(Report {
                id: None,
                report: StateReport::State(state),
            }) => {
                self.reported = Some(state);
            }
            Some(Report {
                id: None,
                report: StateReport::Clear | StateReport::Cut,
            }) => {
                self.reported = None;
            }
            Some(Report { id: Some(_), .. }) | None => {}
        }
    }

    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        intermediate: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        final_byte: char,
    ) {
        if intermediate != Some(b'?') {
            return;
        }
        let enabled = match final_byte {
            'h' => true,
            'l' => false,
            _ => return,
        };
        // A DECSET may carry several modes at once (Claude Code sets the four
        // mouse modes in one sequence), so every parameter is inspected.
        for mode in params.iter().filter_map(|p| p.first().copied()) {
            match mode {
                2026 => self.synchronized_update = enabled,
                1004 => self.wants_focus_events = enabled,
                _ => {}
            }
        }
    }
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Self {
        let (rows, cols) = floored(rows, cols);
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows,
                cols,
                SCROLLBACK_LEN,
                PaneModes::default(),
            ),
            size: (rows, cols),
        }
    }

    /// What the program running in this pane last reported about its own state
    /// (OSC 7501), if it has since it started: this model is made new with each
    /// process. `None` before the first report and after a `clear`.
    pub fn reported_state(&self) -> Option<ReportedState> {
        self.parser.callbacks().reported
    }

    /// Whether the child is part-way through a frame it asked not to be shown
    /// mid-flight (DECSET 2026). The caller decides how long to honour that —
    /// this type has no clock, and a child that dies between the open and the
    /// close would otherwise freeze its pane forever.
    pub fn in_synchronized_update(&self) -> bool {
        self.parser.callbacks().synchronized_update
    }

    /// Whether the child asked to be told when it gains or loses focus
    /// (DECSET 1004), which it answers with `CSI I` / `CSI O`.
    pub fn wants_focus_events(&self) -> bool {
        self.parser.callbacks().wants_focus_events
    }

    /// Take any clipboard payload the child staged with OSC 52, as
    /// `(selection, base64)`. Draining is what makes it a one-shot: the same
    /// copy must not be forwarded to the host terminal on every later poll.
    pub fn take_clipboard(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        self.parser.screen_mut().take_clipboard()
    }

    /// Take whether the child rang BEL since the previous call. The pinned
    /// vt100 fork coalesces however many BEL bytes arrived into this one-shot
    /// fact, which is exactly what the emporium needs for a pane latch.
    pub fn take_audible_bell(&mut self) -> bool {
        self.parser.screen_mut().take_audible_bell()
    }

    /// Feed one chunk of the child's output into the model.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Resize the model. Returns whether the size actually changed (a no-op
    /// resize is not worth forwarding to the PTY).
    pub fn resize(&mut self, rows: u16, cols: u16) -> bool {
        let (rows, cols) = floored(rows, cols);
        if self.size == (rows, cols) {
            return false;
        }
        self.parser.screen_mut().set_size(rows, cols);
        self.size = (rows, cols);
        true
    }

    /// The size the screen actually has, `(rows, cols)`: what was asked for,
    /// raised to [`MIN_ROWS`] x [`MIN_COLS`]. The size the PTY must be given too.
    pub fn size(&self) -> (u16, u16) {
        self.size
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
    /// behavior on its own, not a case this method needs to special-case:
    /// alternate-screen content has no history behind it worth scrolling
    /// into — what would be "back" is whatever was on screen before the app
    /// took over, which is exactly what the alternate screen exists to hide.
    ///
    /// Which children this actually applies to is worth not guessing at.
    /// Codex was assumed to be one of them and is not: raw PTY captures
    /// found no `\x1b[?1049h` at cold start or through a full turn, both
    /// before and after the ConPTY crate swap
    /// (`banto_io::pty::PortablePtyHost`'s doc). It scrolls in the normal
    /// buffer like a shell does, which is the case this scrollback exists
    /// to serve.
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
    fn synchronized_update_opens_and_closes_with_decset_2026() {
        let mut screen = Screen::new(4, 20);
        assert!(!screen.in_synchronized_update());
        screen.process(b"\x1b[?2026h");
        assert!(screen.in_synchronized_update());
        screen.process(b"\x1b[?2026l");
        assert!(!screen.in_synchronized_update());
    }

    #[test]
    fn focus_reporting_is_tracked_separately_from_synchronized_update() {
        let mut screen = Screen::new(4, 20);
        screen.process(b"\x1b[?1004h");
        assert!(screen.wants_focus_events());
        assert!(!screen.in_synchronized_update());
        screen.process(b"\x1b[?1004l");
        assert!(!screen.wants_focus_events());
    }

    #[test]
    fn a_decset_carrying_several_modes_sets_every_one_it_names() {
        // Claude Code enables its four mouse modes in a single sequence, so a
        // reader that only inspected the first parameter would miss the rest.
        let mut screen = Screen::new(4, 20);
        screen.process(b"\x1b[?1004;2026h");
        assert!(screen.wants_focus_events());
        assert!(screen.in_synchronized_update());
    }

    #[test]
    fn modes_vt100_implements_itself_never_reach_the_callback() {
        // DECSET 2004 is handled inside vt100, so it is not reported as
        // unhandled — the same discrimination that makes 2026 and 1004
        // observable here at all. If this ever fails, the callback is being
        // fed modes it must not interpret.
        let mut screen = Screen::new(4, 20);
        screen.process(b"\x1b[?2004h");
        assert!(!screen.in_synchronized_update());
        assert!(!screen.wants_focus_events());
    }

    #[test]
    fn a_clipboard_payload_is_handed_over_once() {
        let mut screen = Screen::new(4, 20);
        screen.process(b"\x1b]52;c;aGVsbG8=\x07");
        let (selection, data) = screen.take_clipboard().expect("staged by OSC 52");
        assert_eq!(selection, b"c");
        assert_eq!(data, b"aGVsbG8=");
        assert!(
            screen.take_clipboard().is_none(),
            "a drained copy must not be forwarded to the host again"
        );
    }

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

    #[test]
    fn audible_bell_is_handed_over_once() {
        let mut screen = Screen::new(4, 20);
        screen.process(b"\x07\x07");
        assert!(screen.take_audible_bell());
        assert!(!screen.take_audible_bell());
    }

    // --- OSC 8 hyperlink close forms: does the pen state actually clear? ---
    //
    // Hand-made synthetic bytes only (repo invariant 2) — no captured session
    // data. Probes which `OSC 8` close spellings the pinned vt100-psmux
    // (github.com/kt81/psmux, `banto/vt100-patches`,
    // `perform.rs:244`'s `[b"8", id_params, uri_rest @ ..]` arm) actually
    // recognizes as "close the current hyperlink" — a form the pattern does
    // not match falls through to `unhandled_osc` and leaves `attrs.link`
    // (the running pen) non-zero, so every later-written cell keeps
    // inheriting the id. Column 0 is always the link's own first character
    // ("L"); the id checked afterward ("P") is one cell past whatever close
    // form was sent.
    //
    // The "leaves the link stuck" tests below assert a KNOWN, CURRENTLY
    // UNFIXED gap in the vendored parser — a bare `OSC 8 ST`/`OSC 8 BEL`
    // close (no semicolons) never reaches `set_hyperlink` at all. That is a
    // bug being documented, not a contract this crate is protecting: the
    // operator has deliberately chosen a different fix (a frame-boundary
    // OSC 8 close in `banto/src/embedded/emporium.rs`, addressing a
    // separate mechanism — the cell diff can transmit an open with its
    // paired close skipped) over patching this parser gap, so the parser
    // gap itself is still open. If this ever gets fixed upstream in the
    // fork, these two assertions should flip to `assert_eq!(.., 0)` along
    // with it — do not "fix" the test to match the bug going forward.

    fn hyperlink_id_at(screen: &Screen, col: u16) -> u32 {
        screen
            .screen()
            .cell(0, col)
            .expect("cell in bounds")
            .hyperlink_id()
    }

    #[test]
    fn standard_two_semicolon_close_terminated_by_st_clears_the_link() {
        let mut screen = Screen::new(1, 20);
        screen.process(b"\x1b]8;;http://example.com\x1b\\L\x1b]8;;\x1b\\P");
        assert_ne!(hyperlink_id_at(&screen, 0), 0, "L must be linked");
        assert_eq!(
            hyperlink_id_at(&screen, 1),
            0,
            "P after a standard ';;' ST-terminated close must not be linked"
        );
    }

    #[test]
    fn standard_two_semicolon_close_terminated_by_bel_clears_the_link() {
        let mut screen = Screen::new(1, 20);
        screen.process(b"\x1b]8;;http://example.com\x1b\\L\x1b]8;;\x07P");
        assert_ne!(hyperlink_id_at(&screen, 0), 0, "L must be linked");
        assert_eq!(
            hyperlink_id_at(&screen, 1),
            0,
            "P after a standard ';;' BEL-terminated close must not be linked"
        );
    }

    #[test]
    fn bare_close_with_no_semicolons_terminated_by_st_leaves_the_link_stuck() {
        let mut screen = Screen::new(1, 20);
        screen.process(b"\x1b]8;;http://example.com\x1b\\L\x1b]8\x1b\\P");
        assert_ne!(hyperlink_id_at(&screen, 0), 0, "L must be linked");
        // Documents the gap named in perform.rs:244's own doc comment: the
        // match arm requires at least two params ("8" plus id_params), so a
        // one-param OSC 8 falls to `unhandled_osc` and never reaches
        // `set_hyperlink`. If this ever starts passing, the gap has closed
        // and this assertion should flip along with it — see the comment
        // above the test group.
        assert_ne!(
            hyperlink_id_at(&screen, 1),
            0,
            "a bare 'OSC 8 ST' close (no semicolons) does not match perform.rs:244's pattern \
             and never reaches set_hyperlink — P stays linked"
        );
    }

    #[test]
    fn bare_close_with_no_semicolons_terminated_by_bel_leaves_the_link_stuck() {
        let mut screen = Screen::new(1, 20);
        screen.process(b"\x1b]8;;http://example.com\x1b\\L\x1b]8\x07P");
        assert_ne!(hyperlink_id_at(&screen, 0), 0, "L must be linked");
        assert_ne!(
            hyperlink_id_at(&screen, 1),
            0,
            "a bare 'OSC 8 BEL' close (no semicolons) does not match perform.rs:244's pattern \
             and never reaches set_hyperlink — P stays linked"
        );
    }

    #[test]
    fn a_close_with_a_populated_id_param_and_empty_uri_still_clears() {
        // OSC 8 ; id=foo ; ST — id_params non-empty, uri empty. The match
        // arm only inspects the joined uri, not id_params, so this should
        // clear same as the standard close.
        let mut screen = Screen::new(1, 20);
        screen.process(b"\x1b]8;;http://example.com\x1b\\L\x1b]8;id=foo;\x1b\\P");
        assert_ne!(hyperlink_id_at(&screen, 0), 0, "L must be linked");
        assert_eq!(
            hyperlink_id_at(&screen, 1),
            0,
            "a populated id_params field must not by itself prevent an empty-uri close"
        );
    }

    #[test]
    fn a_close_split_across_two_process_calls_at_an_awkward_byte_boundary_still_clears() {
        // The close's own bytes (`\x1b]8;;\x1b\\`) are split mid-sequence,
        // as an OS-buffered PTY read could split it. `vte`'s own state
        // machine is what has to reassemble this, not banto or the fork —
        // this documents that the reassembly does happen, not that this
        // repo built it.
        let mut screen = Screen::new(1, 20);
        screen.process(b"\x1b]8;;http://example.com\x1b\\L");
        screen.process(b"\x1b]8;"); // split mid-OSC-8, before the second ';'
        screen.process(b";\x1b\\P");
        assert_ne!(hyperlink_id_at(&screen, 0), 0, "L must be linked");
        assert_eq!(
            hyperlink_id_at(&screen, 1),
            0,
            "a close split across two process() calls must still clear the link"
        );
    }

    #[test]
    fn an_sgr_reset_between_open_and_close_clears_the_link_even_with_no_close_at_all() {
        // No OSC 8 close is ever sent here — only a plain SGR reset
        // (`ESC[0m`), which `Screen::sgr`'s `[0] =>` arm replaces the whole
        // `Attrs` struct with `Attrs::default()`, `link` included.
        let mut screen = Screen::new(1, 20);
        screen.process(b"\x1b]8;;http://example.com\x1b\\L\x1b[0mP");
        assert_ne!(hyperlink_id_at(&screen, 0), 0, "L must be linked");
        assert_eq!(
            hyperlink_id_at(&screen, 1),
            0,
            "a plain SGR reset must clear a stuck link even with no OSC 8 close at all"
        );
    }
}

#[cfg(test)]
mod state_report_tests {
    use super::*;
    use crate::program_state::BlockedKind;

    const WORKING: &[u8] = b"\x1b]7501;state=working:app=codex\x1b\\";

    fn fed(chunks: &[&[u8]]) -> Screen {
        let mut screen = Screen::new(4, 40);
        for chunk in chunks {
            screen.process(chunk);
        }
        screen
    }

    #[test]
    fn a_whole_report_is_taken_in_with_either_terminator() {
        assert_eq!(
            fed(&[WORKING]).reported_state(),
            Some(ReportedState::Working)
        );
        assert_eq!(
            fed(&[b"\x1b]7501;state=idle:app=codex\x07"]).reported_state(),
            Some(ReportedState::Idle)
        );
    }

    #[test]
    fn a_report_split_anywhere_across_two_or_three_chunks_is_still_one_report() {
        let text: &[u8] = b"\x1b]7501;state=blocked:app=codex:kind=permission\x1b\\";
        let expected = Some(ReportedState::Blocked(Some(BlockedKind::Permission)));
        for cut in 1..text.len() {
            let screen = fed(&[&text[..cut], &text[cut..]]);
            assert_eq!(screen.reported_state(), expected, "cut at {cut}");
        }
        for first in 1..text.len() - 1 {
            for second in first + 1..text.len() {
                let screen = fed(&[&text[..first], &text[first..second], &text[second..]]);
                assert_eq!(screen.reported_state(), expected, "cuts {first},{second}");
            }
        }
        // One byte at a time is the extreme.
        let bytes: Vec<&[u8]> = text.chunks(1).collect();
        assert_eq!(fed(&bytes).reported_state(), expected);
    }

    #[test]
    fn fields_in_either_order_and_clear_and_the_claude_query() {
        assert_eq!(
            fed(&[b"\x1b]7501;kind=question:app=codex:state=blocked\x07"]).reported_state(),
            Some(ReportedState::Blocked(Some(BlockedKind::Question)))
        );
        let mut screen = fed(&[WORKING]);
        screen.process(b"\x1b]7501;state=clear\x1b\\");
        assert_eq!(screen.reported_state(), None, "clear");

        // Claude Code's query-shaped sequence and an unknown state change nothing.
        let mut screen = fed(&[WORKING]);
        screen.process(b"\x1b]7501;?\x07");
        screen.process(b"\x1b]7501;state=sleeping:app=codex\x1b\\");
        assert_eq!(screen.reported_state(), Some(ReportedState::Working));
        assert_eq!(fed(&[b"\x1b]7501;?\x07"]).reported_state(), None);
    }

    #[test]
    fn an_over_long_payload_is_not_a_report_and_the_next_one_still_is() {
        let mut screen = Screen::new(4, 40);
        let mut long = b"\x1b]7501;state=idle:app=codex:".to_vec();
        long.extend(std::iter::repeat_n(b'x', 5000));
        long.extend_from_slice(b"\x1b\\");
        screen.process(&long);
        assert_eq!(screen.reported_state(), None);
        screen.process(WORKING);
        assert_eq!(screen.reported_state(), Some(ReportedState::Working));
    }

    #[test]
    fn a_query_and_a_report_in_either_order_leave_the_report() {
        let query = b"\x1b]7501;?\x07".as_slice();
        let query_with_lists = b"\x1b]7501;?:states=idle,working:kinds=question\x1b\\".as_slice();
        for query in [query, query_with_lists] {
            assert_eq!(
                fed(&[query, WORKING]).reported_state(),
                Some(ReportedState::Working),
                "query first"
            );
            assert_eq!(
                fed(&[WORKING, query]).reported_state(),
                Some(ReportedState::Working),
                "report first"
            );
            // Fed as one chunk, with nothing between them.
            let mut joined = query.to_vec();
            joined.extend_from_slice(WORKING);
            assert_eq!(
                fed(&[&joined]).reported_state(),
                Some(ReportedState::Working)
            );
        }
    }

    /// What the parser (vte under `vt100`) does with a report that touches
    /// another escape sequence. Each row is the behaviour of the pinned parser
    /// today, not something this code decides; a parser upgrade that changes one
    /// should be looked at, not silently followed.
    #[test]
    fn a_report_next_to_other_escape_sequences_is_read_as_the_parser_reads_them() {
        use ReportedState::{Idle, Working};
        let cases: &[(&str, &[u8], Option<ReportedState>)] = &[
            (
                "straight after a CSI",
                b"\x1b[31m\x1b[1;1H\x1b]7501;state=working\x07",
                Some(Working),
            ),
            (
                "straight before one",
                b"\x1b]7501;state=working\x07\x1b[31m",
                Some(Working),
            ),
            (
                "after an OSC 0 title ended by BEL",
                b"\x1b]0;a title\x07\x1b]7501;state=working\x07",
                Some(Working),
            ),
            (
                "after an OSC 0 title ended by ST",
                b"\x1b]0;a title\x1b\\\x1b]7501;state=idle\x1b\\",
                Some(Idle),
            ),
            (
                "inside the title's own text it is just text",
                b"\x1b]0;7501;state=working\x07",
                None,
            ),
            (
                "after an OSC 0 title left open: the ESC ends it and the next starts",
                b"\x1b]0;a titl\x1b]7501;state=working\x07",
                Some(Working),
            ),
            (
                "cut short by a CSI: the ESC ends it, with what it had so far",
                b"\x1b]7501;state=work\x1b[31m",
                None,
            ),
            (
                "cut short by a CSI after a whole state: the ESC ends it and it counts",
                b"\x1b]7501;state=working\x1b[31m",
                Some(Working),
            ),
            (
                "cancelled by CAN after a whole state: the parser still dispatches it",
                b"]7501;state=working",
                Some(Working),
            ),
            (
                "cancelled by SUB part-way through the state",
                b"]7501;state=working",
                None,
            ),
            (
                "in a DCS, as its data (no ESC, so the DCS does not end)",
                b"\x1bPq]7501;state=working\x07\x1b\\",
                None,
            ),
            (
                "right after a DCS ended by ST",
                b"\x1bPqdata\x1b\\\x1b]7501;state=working\x07",
                Some(Working),
            ),
            (
                "right after a DCS that an ESC cut short",
                b"\x1bPqdata\x1b]7501;state=working\x07",
                Some(Working),
            ),
            (
                "in an APC, as its data",
                b"\x1b_]7501;state=working\x07\x1b\\",
                None,
            ),
            (
                "right after an APC that an ESC cut short",
                b"\x1b_x\x1b]7501;state=working\x07",
                Some(Working),
            ),
        ];
        for (what, bytes, expected) in cases {
            assert_eq!(fed(&[bytes]).reported_state(), *expected, "{what}");
        }
    }

    /// A report whose `state` field comes after `semicolons` `;` in the payload.
    fn after_semicolons(semicolons: usize, tail: &str) -> Vec<u8> {
        format!("\x1b]7501;x{}{tail}\x07", ";".repeat(semicolons)).into_bytes()
    }

    const IDLE: &[u8] = b"\x1b]7501;state=idle\x07";

    #[test]
    fn the_parsers_parameter_limit_loses_a_state_after_the_fifteenth_semicolon_and_no_earlier() {
        // vte keeps the number and fifteen more parameters: the text after the
        // sixteenth `;` of the whole sequence, which is the fifteenth of the
        // payload, never arrives. The state field in the sixteenth parameter
        // does; in the seventeenth it does not, and the report is taken as cut
        // short, which ends what the pane reported before.
        let state = ":state=working";
        let mut at_the_edge = fed(&[IDLE]);
        at_the_edge.process(&after_semicolons(14, state));
        assert_eq!(at_the_edge.reported_state(), Some(ReportedState::Working));

        let mut past_it = fed(&[IDLE]);
        assert_eq!(past_it.reported_state(), Some(ReportedState::Idle));
        past_it.process(&after_semicolons(15, state));
        assert_eq!(past_it.reported_state(), None, "cut: nothing is believed");
        past_it.process(&after_semicolons(40, state));
        assert_eq!(past_it.reported_state(), None);
        // And the next whole report is taken as usual.
        past_it.process(WORKING);
        assert_eq!(past_it.reported_state(), Some(ReportedState::Working));
    }

    #[test]
    fn a_cut_report_that_still_carries_its_state_is_applied() {
        let mut screen = fed(&[IDLE]);
        let early = format!("\x1b]7501;state=working:title=t{}\x07", ";x".repeat(30));
        screen.process(early.as_bytes());
        assert_eq!(screen.reported_state(), Some(ReportedState::Working));
        let early_clear = format!("\x1b]7501;title=t:state=clear:y{}\x07", ";x".repeat(30));
        screen.process(early_clear.as_bytes());
        assert_eq!(screen.reported_state(), None);
    }

    #[test]
    fn a_query_and_a_short_unknown_payload_change_nothing_however_many_separators() {
        let mut screen = fed(&[WORKING]);
        screen.process(b"\x1b]7501;?\x07");
        screen.process(b"\x1b]7501;future=thing\x07");
        screen.process(b"\x1b]7501;progress=40:msg=aGk=\x07");
        // A query is never a report, cut short or not.
        let long_query = format!("\x1b]7501;?:states=idle{}\x07", ";working".repeat(30));
        screen.process(long_query.as_bytes());
        assert_eq!(screen.reported_state(), Some(ReportedState::Working));
    }

    #[test]
    fn a_stateless_payload_is_a_cut_only_once_it_fills_every_parameter_the_parser_keeps() {
        // Fifteen parameters in all (the number and fourteen more) lost nothing
        // and says nothing; sixteen may have lost something, and cannot say.
        let payload =
            |parameters: usize| format!("]7501;x{}", ";x".repeat(parameters - 1)).into_bytes();
        let mut screen = fed(&[WORKING]);
        screen.process(&payload(14));
        assert_eq!(screen.reported_state(), Some(ReportedState::Working));
        screen.process(&payload(15));
        assert_eq!(screen.reported_state(), None);
    }

    #[test]
    fn a_cut_report_that_names_a_sub_record_leaves_the_root_state_alone() {
        // `id=` arrived, so the report was about a sub-record, whatever else
        // was lost.
        let mut screen = fed(&[WORKING]);
        screen.process(&after_semicolons(20, ":state=idle"));
        assert_eq!(screen.reported_state(), None, "no id arrived: the root");

        let mut screen = fed(&[WORKING]);
        let named = format!("\x1b]7501;id=build:x{}:state=idle\x07", ";".repeat(20));
        screen.process(named.as_bytes());
        assert_eq!(screen.reported_state(), Some(ReportedState::Working));
    }

    #[test]
    fn a_report_about_a_sub_record_leaves_the_root_state_alone() {
        let mut screen = fed(&[WORKING]);
        screen.process(b"\x1b]7501;state=blocked:id=build/tests:kind=permission\x1b\\");
        screen.process(b"\x1b]7501;state=idle:id=lint\x07");
        assert_eq!(screen.reported_state(), Some(ReportedState::Working));
        // Clearing a sub-record is not clearing the pane.
        screen.process(b"\x1b]7501;state=clear:id=build/tests\x1b\\");
        screen.process(b"\x1b]7501;id=lint:state=clear\x1b\\");
        assert_eq!(screen.reported_state(), Some(ReportedState::Working));
        // The root record still answers to a report without an id.
        screen.process(b"\x1b]7501;state=done:app=codex\x1b\\");
        assert_eq!(screen.reported_state(), Some(ReportedState::Done));
    }

    #[test]
    fn the_feature_detection_form_with_its_lists_is_not_a_report() {
        let mut screen = fed(&[WORKING]);
        screen.process(b"\x1b]7501;?:states=idle,working,blocked:kinds=permission\x07");
        assert_eq!(screen.reported_state(), Some(ReportedState::Working));
    }

    #[test]
    fn the_protocols_four_thousand_ninety_six_bytes_are_read_and_one_more_is_not() {
        let report = |payload_len: usize| {
            let head = "state=idle:x=";
            let mut bytes = b"\x1b]7501;".to_vec();
            bytes.extend_from_slice(head.as_bytes());
            bytes.extend(std::iter::repeat_n(b'y', payload_len - head.len()));
            bytes.extend_from_slice(b"\x1b\\");
            bytes
        };
        assert_eq!(
            fed(&[&report(4096)]).reported_state(),
            Some(ReportedState::Idle)
        );
        assert_eq!(fed(&[&report(4097)]).reported_state(), None);
    }

    #[test]
    fn sequences_back_to_back_end_with_the_last_and_each_counts() {
        let mut both = Vec::new();
        both.extend_from_slice(b"\x1b]7501;state=working:app=codex\x1b\\");
        both.extend_from_slice(b"\x1b]7501;state=done:app=codex\x1b\\");
        let screen = fed(&[&both]);
        assert_eq!(screen.reported_state(), Some(ReportedState::Done));
    }

    #[test]
    fn the_bytes_still_reach_the_terminal_model_untouched() {
        // Text around the sequence is drawn exactly as it would be without it.
        let mut with = Screen::new(4, 40);
        with.process(b"ab\x1b]7501;state=idle:app=codex\x1b\\cd\r\n\x1b[31mred");
        let mut without = Screen::new(4, 40);
        without.process(b"abcd\r\n\x1b[31mred");
        assert_eq!(
            with.screen().contents(),
            without.screen().contents(),
            "contents"
        );
        assert_eq!(with.screen().contents(), "abcd\nred");
        assert_eq!(
            with.screen().cursor_position(),
            without.screen().cursor_position()
        );
    }

    #[test]
    fn a_new_screen_has_reported_nothing() {
        let screen = Screen::new(4, 40);
        assert_eq!(screen.reported_state(), None);
    }
}

#[cfg(test)]
mod floor_tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    /// Classes of child output that have made a terminal model panic or could:
    /// text that wraps, double-width glyphs (also at the last column), scrolling,
    /// insert and delete, regions of one and two rows, reverse index, absolute
    /// moves far off the grid, tabs, erase, the alternate screen, origin mode.
    const INPUTS: &[(&str, &str)] = &[
        ("wrap", "abcdefghijklmnopqrstuvwxyz0123456789"),
        ("newlines", "a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nh\r\n"),
        ("bare LF", "a\nb\nc\nd\ne\nf\n"),
        (
            "wide",
            "\u{3042}\u{3044}\u{3046}\u{3048}\u{304a}\u{304b}\u{304d}",
        ),
        ("text then wide", "ab\u{3042}cd\u{3044}"),
        ("emoji", "\u{1F600}\u{1F600}\u{1F600}\u{1F600}"),
        ("combining", "e\u{301}e\u{301}e\u{301}xyz"),
        ("vs16", "\u{2764}\u{fe0f}\u{2764}\u{fe0f}\u{2764}\u{fe0f}"),
        ("scroll up", "abc\x1b[3Sdef"),
        ("scroll down", "abc\x1b[3Tdef"),
        ("insert lines", "a\r\nb\x1b[2Lc"),
        ("delete lines", "a\r\nb\x1b[2Mc"),
        ("region 1;1", "\x1b[1;1rabcdef\r\nxyz"),
        ("region 2;2", "\x1b[2;2rabcdef\r\nxyz"),
        ("region 1;2", "\x1b[1;2rabcdef\r\nxyz\r\nuvw"),
        ("reverse index", "\x1bMabc\x1bM\x1bM"),
        ("clear and home", "abc\x1b[2J\x1b[Hdef"),
        ("move far", "\x1b[999;999Habcdef"),
        ("tabs", "\t\t\t\tabc\t\t"),
        ("ich dch ech", "abcdef\x1b[3@\x1b[3P\x1b[3X"),
        ("alternate screen", "\x1b[?1049habcdef\r\nxyz\x1b[?1049l"),
        ("save restore", "abc\x1b7def\x1b8ghi"),
        ("backspace", "\x08\x08\x08abc\x08\x08\x08\x08"),
        ("erase", "abc\x1b[2K\x1b[1J\x1b[0J\x1b[1K"),
        ("sgr", "\x1b[31mabcdef\x1b[0m\r\nghijkl"),
        ("no autowrap", "\x1b[?7labcdefghij"),
        ("origin mode", "\x1b[?6h\x1b[1;1Habc\r\ndef"),
    ];

    /// Which inputs panic a bare `vt100` parser of `rows` x `cols`, built that
    /// size, shrunk to it from 24x80 after some output, or grown from it to
    /// 24x80 mid-stream. The `vt100` parser itself, not `Screen`, which floors.
    fn panicking(rows: u16, cols: u16) -> Vec<&'static str> {
        let panics = |run: &dyn Fn()| catch_unwind(AssertUnwindSafe(run)).is_err();
        let mut failed = Vec::new();
        for (name, input) in INPUTS {
            let bytes = input.as_bytes();
            let fresh = || {
                let mut p = vt100::Parser::new(rows, cols, SCROLLBACK_LEN);
                p.process(bytes);
                p.process(bytes);
            };
            let shrunk = || {
                let mut p = vt100::Parser::new(24, 80, SCROLLBACK_LEN);
                p.process(b"hello\r\nworld\r\n");
                p.screen_mut().set_size(rows, cols);
                p.process(bytes);
                p.process(bytes);
            };
            let grown = || {
                let mut p = vt100::Parser::new(rows, cols, SCROLLBACK_LEN);
                p.process(bytes);
                p.screen_mut().set_size(24, 80);
                p.process(bytes);
            };
            if panics(&fresh) || panics(&shrunk) || panics(&grown) {
                failed.push(*name);
            }
        }
        failed
    }

    /// The floor, against the parser the product ships: nothing panics at or
    /// above it, on either axis, up to sizes well past it.
    #[test]
    fn a_screen_at_or_above_the_floor_never_panics_in_the_real_parser() {
        for rows in MIN_ROWS..=6 {
            for cols in MIN_COLS..=6 {
                assert_eq!(
                    panicking(rows, cols),
                    Vec::<&str>::new(),
                    "{rows}x{cols} is at or above the floor"
                );
            }
        }
    }

    /// What breaks below the floor today, recorded so that a parser upgrade that
    /// fixes any of it is noticed (and the floor then reconsidered), not
    /// silently followed. Only what holds in both build profiles is asserted
    /// unconditionally; the rest is a debug build's overflow checks.
    #[test]
    fn what_panics_below_the_floor_is_recorded_so_a_parser_upgrade_is_noticed() {
        // One row: text that wraps panics, in a release build too (the
        // subtraction wraps and the `unwrap()` after it panics).
        for cols in 1..=6 {
            assert!(
                panicking(1, cols).contains(&"wrap"),
                "wrapping text on 1x{cols}"
            );
        }
        if cfg!(debug_assertions) {
            // One column: a double-width glyph underflows `cols - width`.
            for rows in MIN_ROWS..=6 {
                let failed = panicking(rows, 1);
                assert_eq!(
                    failed,
                    ["wide", "text then wide", "emoji"],
                    "{rows}x1 in a debug build"
                );
            }
            // No rows, or no columns: panics, whatever is printed.
            for size in 0..=3 {
                assert_eq!(panicking(0, size).len(), INPUTS.len(), "0x{size}");
                assert_eq!(panicking(size, 0).len(), INPUTS.len(), "{size}x0");
            }
        }
    }

    #[test]
    fn a_screen_never_takes_a_size_below_the_floor() {
        for (rows, cols) in [(0, 0), (1, 1), (1, 80), (24, 1), (0, 80), (24, 0)] {
            let screen = Screen::new(rows, cols);
            assert_eq!(
                screen.size(),
                (rows.max(MIN_ROWS), cols.max(MIN_COLS)),
                "new {rows}x{cols}"
            );
            assert_eq!(screen.screen().size(), screen.size());
        }
        let mut screen = Screen::new(24, 80);
        assert!(screen.resize(1, 1), "24x80 to the floor is a change");
        assert_eq!(screen.size(), (MIN_ROWS, MIN_COLS));
        assert_eq!(screen.screen().size(), (MIN_ROWS, MIN_COLS));
        assert!(!screen.resize(0, 0), "asking for less than the floor again");
        assert!(
            !screen.resize(1, 2),
            "is no change to a screen already there"
        );
        assert!(screen.resize(30, 100));
        assert_eq!(screen.size(), (30, 100));
    }
}
