//! Reading what is on a hosted Claude pane's input line, from the screen
//! banto already holds for it.
//!
//! A fact about the pane that Claude publishes nowhere else — whether
//! anything is typed at its prompt — is on the painted screen and nowhere
//! else. This is the one place the core reads it, under the fence in
//! `docs/DISCIPLINE.md` §3 ("Reading the painted screen"): at the moment of a
//! decision, never to estimate state, and closed (`Unrecognized`) whenever the
//! screen is not exactly the shape this was measured on.
//!
//! Measured 2026-11-10 on a throwaway Claude Code session hosted by banto
//! (64x183 pane, alternate screen): the input is a block framed above and
//! below by a row of U+2500 across the whole pane width. The first row of the
//! block starts with U+276F then a no-break space, and a continuation row
//! (after Shift+Enter) starts with two plain spaces. An empty block is the
//! glyph row alone, with a dim placeholder after the glyph and the cursor in
//! column 2; typed text is drawn in normal attributes with the cursor after
//! it. The block grows upward as lines are added and its bottom rule stays
//! put; after Ctrl+C the whole block sat two rows lower for about 0.8s while
//! the footer was a single `Press Ctrl-C again to exit` row. The cursor was
//! visible throughout, so its visibility is not read. Nothing about this is
//! documented by Claude Code; the rule below fails closed instead of
//! guessing when any of it stops being true.

use crate::screen::Screen;

/// What the input line of a pane holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputLine {
    /// Recognised, and nothing is typed (the line is blank or shows only a
    /// dim placeholder).
    Empty,
    /// Recognised, and something is typed — including a newline, and text
    /// that came in by a route banto never saw (recalled history).
    HasText,
    /// Not the shape this knows how to read. Callers fall back to what they
    /// knew before this existed.
    Unrecognized,
}

/// The prompt glyph that opens the input block's first row.
const PROMPT_GLYPH: &str = "\u{276f}";
/// The box-drawing horizontal the block's frame is made of.
const RULE_GLYPH: &str = "\u{2500}";
/// Where the cursor sits on an empty prompt row: after the glyph and the
/// no-break space that follows it.
const EMPTY_CURSOR_COL: u16 = 2;

/// Where the block is on the screen, rows inclusive of the prompt row and
/// exclusive of the bottom rule.
struct Block {
    prompt_row: u16,
    bottom_rule_row: u16,
    cursor: (u16, u16),
}

fn is_blank(contents: &str) -> bool {
    contents.chars().all(|c| c == ' ' || c == '\u{a0}')
}

fn is_full_width_rule(screen: &vt100::Screen, row: u16, cols: u16) -> bool {
    cols > 0
        && (0..cols).all(|col| {
            screen
                .cell(row, col)
                .is_some_and(|cell| cell.contents() == RULE_GLYPH)
        })
}

/// Find the framed input block the cursor is in.
///
/// Walks up from the cursor row to the nearest row whose first cell is the
/// prompt glyph; the row above it must be a full-width rule, and so must the
/// first row below it that is one (the block's bottom edge), with the cursor
/// inside the block. `None` for anything else, and while the child is
/// mid-frame in a synchronized update (DECSET 2026) or the view is scrolled
/// back into history, when the cells are not the live ones.
fn locate(pane: &Screen) -> Option<Block> {
    if pane.in_synchronized_update() {
        return None;
    }
    let screen = pane.screen();
    if screen.scrollback() != 0 {
        return None;
    }
    let (rows, cols) = screen.size();
    let cursor = screen.cursor_position();
    let prompt_row = (0..=cursor.0.min(rows.checked_sub(1)?))
        .rev()
        .find(|&row| {
            screen
                .cell(row, 0)
                .is_some_and(|cell| cell.contents() == PROMPT_GLYPH)
        })?;
    if prompt_row == 0 || !is_full_width_rule(screen, prompt_row - 1, cols) {
        return None;
    }
    let bottom_rule_row =
        (prompt_row + 1..rows).find(|&row| is_full_width_rule(screen, row, cols))?;
    (cursor.0 < bottom_rule_row).then_some(Block {
        prompt_row,
        bottom_rule_row,
        cursor,
    })
}

/// Read the pane's input line. See the module doc for what is recognised.
///
/// `Empty` needs all of: a one-row block, every cell after the glyph blank
/// or dim, and the cursor at [`EMPTY_CURSOR_COL`]. A recognised block that
/// fails any of those is `HasText`: a multi-row block means a newline was
/// typed, and a cursor anywhere else means the line is not the bare prompt.
pub fn read_input_line(pane: &Screen) -> InputLine {
    let Some(block) = locate(pane) else {
        return InputLine::Unrecognized;
    };
    let screen = pane.screen();
    let (_, cols) = screen.size();
    let one_row = block.bottom_rule_row == block.prompt_row + 1;
    let rest_is_blank_or_dim = (1..cols).all(|col| {
        screen
            .cell(block.prompt_row, col)
            .is_none_or(|cell| is_blank(cell.contents()) || cell.dim())
    });
    if one_row && rest_is_blank_or_dim && block.cursor.1 == EMPTY_CURSOR_COL {
        InputLine::Empty
    } else {
        InputLine::HasText
    }
}

/// The text typed in the input block with all whitespace removed, or `None`
/// when the block is not recognised. Whitespace goes because it is what a
/// wrapped or indented row adds without being typed, and so cannot be
/// compared; dim cells (the placeholder) are not typed text and are left
/// out.
pub fn typed_text_without_whitespace(pane: &Screen) -> Option<String> {
    let block = locate(pane)?;
    let screen = pane.screen();
    let (_, cols) = screen.size();
    let mut text = String::new();
    for row in block.prompt_row..block.bottom_rule_row {
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if col == 0 && row == block.prompt_row && cell.contents() == PROMPT_GLYPH {
                continue;
            }
            if cell.dim() {
                continue;
            }
            text.extend(cell.contents().chars().filter(|c| !c.is_whitespace()));
        }
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: u16 = 24;
    const COLS: u16 = 80;

    fn rule() -> String {
        RULE_GLYPH.repeat(COLS as usize)
    }

    /// Move to 1-based `row`/`col` and write `text`.
    fn at(row: u16, col: u16, text: &str) -> String {
        format!("\x1b[{row};{col}H{text}")
    }

    /// A pane showing an empty prompt block: rule, `❯` + no-break space + dim
    /// placeholder, rule, a footer row; the cursor after the glyph. Rows are
    /// 1-based in escape sequences, so the prompt row is `prompt` (0-based)
    /// + 1.
    fn empty_frame(prompt: u16) -> String {
        let r = prompt + 1;
        format!(
            "\x1b[2J{}{}{}{}",
            at(r - 1, 1, &rule()),
            at(r, 1, "\u{276f}\u{a0}\x1b[2mTry something\x1b[0m"),
            at(r + 1, 1, &rule()),
            at(r + 2, 1, "  ? for shortcuts  (shift+tab to cycle)"),
        ) + &at(r, 3, "")
    }

    fn pane(bytes: &str) -> Screen {
        let mut screen = Screen::new(ROWS, COLS);
        screen.process(bytes.as_bytes());
        screen
    }

    #[test]
    fn an_empty_prompt_with_a_dim_placeholder_is_empty() {
        assert_eq!(read_input_line(&pane(&empty_frame(19))), InputLine::Empty);
    }

    #[test]
    fn an_empty_prompt_with_nothing_after_the_glyph_is_empty() {
        let frame = format!(
            "\x1b[2J{}{}{}{}",
            at(19, 1, &rule()),
            at(20, 1, "\u{276f}\u{a0}"),
            at(21, 1, &rule()),
            at(20, 3, ""),
        );
        assert_eq!(read_input_line(&pane(&frame)), InputLine::Empty);
    }

    #[test]
    fn typed_text_in_normal_attributes_is_has_text() {
        let frame = empty_frame(19) + &at(20, 1, "\x1b[K\u{276f}\u{a0}abc");
        assert_eq!(read_input_line(&pane(&frame)), InputLine::HasText);
    }

    #[test]
    fn a_multi_row_block_is_has_text_even_with_the_cursor_on_a_blank_row() {
        // Shift+Enter grows the block upward; the bottom rule stays put and
        // continuation rows start with two plain spaces.
        let frame = format!(
            "\x1b[2J{}{}{}{}{}",
            at(18, 1, &rule()),
            at(19, 1, "\u{276f}\u{a0}line1"),
            at(20, 1, "  line2"),
            at(21, 1, &rule()),
            at(20, 3, ""),
        );
        let screen = pane(&frame);
        assert_eq!(read_input_line(&screen), InputLine::HasText);

        let blank_last_row = format!(
            "\x1b[2J{}{}{}{}{}",
            at(18, 1, &rule()),
            at(19, 1, "\u{276f}\u{a0}line1"),
            at(20, 1, "  "),
            at(21, 1, &rule()),
            at(20, 3, ""),
        );
        assert_eq!(read_input_line(&pane(&blank_last_row)), InputLine::HasText);
    }

    #[test]
    fn a_block_drawn_two_rows_lower_is_still_found() {
        // Just after Ctrl+C the footer shrank to one row and the whole block
        // sat lower; nothing depends on the block's row.
        let frame = empty_frame(21) + &at(24, 1, "  Press Ctrl-C again to exit") + &at(22, 3, "");
        assert_eq!(read_input_line(&pane(&frame)), InputLine::Empty);
    }

    #[test]
    fn a_prompt_glyph_row_that_is_not_framed_is_unrecognized() {
        let no_top = format!(
            "\x1b[2J{}{}{}",
            at(20, 1, "\u{276f}\u{a0}abc"),
            at(21, 1, &rule()),
            at(20, 6, ""),
        );
        assert_eq!(read_input_line(&pane(&no_top)), InputLine::Unrecognized);

        let no_bottom = format!(
            "\x1b[2J{}{}{}",
            at(19, 1, &rule()),
            at(20, 1, "\u{276f}\u{a0}abc"),
            at(20, 6, ""),
        );
        assert_eq!(read_input_line(&pane(&no_bottom)), InputLine::Unrecognized);
    }

    #[test]
    fn a_rule_that_is_not_the_full_width_does_not_frame_the_block() {
        let short = RULE_GLYPH.repeat(COLS as usize - 1);
        let frame = format!(
            "\x1b[2J{}{}{}{}",
            at(19, 1, &short),
            at(20, 1, "\u{276f}\u{a0}\x1b[2mTry\x1b[0m"),
            at(21, 1, &rule()),
            at(20, 3, ""),
        );
        assert_eq!(read_input_line(&pane(&frame)), InputLine::Unrecognized);
    }

    #[test]
    fn a_screen_with_no_prompt_row_is_unrecognized() {
        assert_eq!(read_input_line(&pane("")), InputLine::Unrecognized);
        let other = format!("\x1b[2J{}{}", at(5, 1, &rule()), at(6, 1, "hello"));
        assert_eq!(read_input_line(&pane(&other)), InputLine::Unrecognized);
    }

    #[test]
    fn a_cursor_outside_the_block_is_unrecognized() {
        let frame = empty_frame(19) + &at(23, 1, "");
        assert_eq!(read_input_line(&pane(&frame)), InputLine::Unrecognized);
    }

    #[test]
    fn a_view_scrolled_back_into_history_is_unrecognized() {
        // 40 lines of output first, so there is scrollback to scroll into,
        // then the prompt block drawn at the bottom.
        let history: String = (0..40).map(|i| format!("line {i}\r\n")).collect();
        let mut screen = pane(&(history + &empty_frame(19)));
        assert_eq!(read_input_line(&screen), InputLine::Empty);

        screen.scroll(3);
        assert_ne!(
            screen.scrollback(),
            0,
            "fixture check: really scrolled back"
        );
        assert_eq!(read_input_line(&screen), InputLine::Unrecognized);
    }

    #[test]
    fn a_synchronized_update_in_progress_is_unrecognized() {
        let screen = pane(&(empty_frame(19) + "\x1b[?2026h"));
        assert!(screen.in_synchronized_update());
        assert_eq!(read_input_line(&screen), InputLine::Unrecognized);

        let closed = pane(&(empty_frame(19) + "\x1b[?2026h\x1b[?2026l"));
        assert_eq!(read_input_line(&closed), InputLine::Empty);
    }

    #[test]
    fn a_dim_run_that_starts_away_from_the_cursor_column_is_has_text() {
        // The placeholder is dim and the cursor is at column 2 when it is
        // there; the same dim text with the cursor elsewhere is not the bare
        // prompt.
        let frame = empty_frame(19) + &at(20, 12, "");
        assert_eq!(read_input_line(&pane(&frame)), InputLine::HasText);
    }

    #[test]
    fn typed_text_drops_the_glyph_the_indent_and_the_placeholder() {
        let multi = format!(
            "\x1b[2J{}{}{}{}{}",
            at(18, 1, &rule()),
            at(19, 1, "\u{276f}\u{a0}[banto] This pane"),
            at(20, 1, "  just started."),
            at(21, 1, &rule()),
            at(20, 17, ""),
        );
        assert_eq!(
            typed_text_without_whitespace(&pane(&multi)).as_deref(),
            Some("[banto]Thispanejuststarted.")
        );
        assert_eq!(
            typed_text_without_whitespace(&pane(&empty_frame(19))).as_deref(),
            Some("")
        );
        assert_eq!(typed_text_without_whitespace(&pane("")), None);
    }
}
