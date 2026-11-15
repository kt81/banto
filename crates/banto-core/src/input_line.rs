//! Reading what is on a hosted Claude pane's input line, from the screen
//! banto already holds for it.
//!
//! A fact about the pane that Claude publishes nowhere else — whether
//! anything is typed at its prompt — is on the painted screen and nowhere
//! else. This is the one place the core reads it, under the fence in
//! `docs/DISCIPLINE.md` §3 ("Reading the painted screen"): at the moment of a
//! decision, never to estimate state, and closed (`Unrecognized`) whenever the
//! screen is not a shape it recognises. Which of the recognised shapes were
//! measured, which are allowed for by construction and which are only a margin
//! is set out below.
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
//!
//! Measured later, on a recording of one Director pane's screen over about 100
//! minutes, replayed tick by tick against this reader: one pane's input box
//! carried a short label inside its top rule. That pane belonged to the one
//! session in the run whose transcript holds `agent-name` records; a session
//! renamed only with `/rename` (`custom-title` records, no `agent-name`) had the
//! plain border. The cause was then observed (2026-11-15, Claude Code 2.1.325):
//! `/rename` on a long-lived session whose transcript held only `custom-title`
//! records, renamed under an earlier Claude Code, added `agent-name` records and
//! drew the label in that session's input box at once. So the current `/rename`
//! writes `agent-name` and draws the label, and an earlier version's wrote only
//! `custom-title` and drew none, which is why two renamed Directors differed.
//!
//! On the recording, the row above the prompt row was a rule run from column 0,
//! one blank, the label (one word of 7 plain cells in one colour), one blank and
//! a single rule cell in the last column, at pane widths 81 and 84 alike (rule
//! runs of 71 and 74 cells), so the label sits against the right edge whatever
//! the width. Every other pane in the same recording had the plain full-width
//! rule. Before the label was allowed for, that one pane read `Unrecognized` on
//! essentially every tick of its life, whatever its stage, focus or size, for no
//! other reason than this row. The label's own text was not looked at, and
//! nothing here reads it.
//! What was measured is only that one word; blanks between words and wide
//! characters are allowed for by construction (see [`is_input_top_border`]).
//! The consequence is accepted: the label's content is not examined, so any short
//! text in that position reads the same as that label. What protects a nudge is the
//! rest of this reader's conditions (the prompt glyph in column 0 of the cursor
//! row, the full-width rule below, the cursor inside the block) and the member
//! being idle and not waiting.

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

/// What one cell of a would-be border row is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BorderCell {
    Rule,
    Blank,
    /// The second cell of a wide character, as the screen says it
    /// (`is_wide_continuation`), not inferred from its contents being empty.
    WideTail,
    Other,
}

/// The row above the prompt row: the input box's top border. Either the plain
/// full-width rule, or the rule with a label in it:
///
/// ```text
/// rule x N, blank, label region, blank, rule x 1
/// ```
///
/// where N is at least half the width, the region begins and ends with a cell
/// that is neither blank nor rule, holds no rule cell, and may hold single blanks
/// between words and the second cells of wide characters. A run of two or more
/// blanks inside the region is rejected: a label does not need one, and it keeps
/// "label, gap, more text" out. No second region can exist, since a rule cell
/// ends the leading run and the label must follow it directly.
///
/// What is measured and what is not (see the module doc): the one word of 7
/// plain cells, the one blank each side and the single trailing rule cell, at
/// widths 81 and 84. Blanks between words and wide characters are extension by
/// construction, not measurement: a second session on the measuring machine
/// whose transcript also holds `agent-name` records had a label of 20 ASCII cells
/// with a space in it, and the text is free in any script, so a reader that took
/// only one plain word would give the next Director with such a label the same
/// `Unrecognized`. On 2026-11-15 a session renamed to a name with wide
/// characters showed the label on a real pane, and its member then idled for two
/// minutes without the relay's "cannot read the input line" notice, which is due
/// after 60 s (`INPUT_LINE_UNREADABLE_AFTER`) of an idle, unrecognised reading.
/// The screen itself was not replayed
/// against this reader, so that shape is observed to be read, not measured cell
/// by cell. The half-width floor is a margin too: what was seen put the label in
/// the last eighth, and half keeps a title at the left or in the middle of a
/// dialog border, a row of text between two rule cells and a mostly blank row
/// out, since none of those is the input box. It caps the label at `width / 2 - 3`
/// cells (wide characters count two); a longer label reads `Unrecognized`.
fn is_input_top_border(screen: &vt100::Screen, row: u16, cols: u16) -> bool {
    if is_full_width_rule(screen, row, cols) {
        return true;
    }
    let kinds: Vec<BorderCell> = (0..cols)
        .map(|col| match screen.cell(row, col) {
            Some(cell) if cell.contents() == RULE_GLYPH => BorderCell::Rule,
            Some(cell) if cell.is_wide_continuation() => BorderCell::WideTail,
            Some(cell) if !is_blank(cell.contents()) => BorderCell::Other,
            _ => BorderCell::Blank,
        })
        .collect();
    let width = kinds.len();
    if width < 5 || kinds[width - 1] != BorderCell::Rule || kinds[width - 2] != BorderCell::Blank {
        return false;
    }
    // The last rule cell before the trailing one ends the leading run; a rule
    // cell inside the label would end it early and leave label cells in it.
    let Some(last_rule) = kinds[..width - 1]
        .iter()
        .rposition(|&kind| kind == BorderCell::Rule)
    else {
        return false;
    };
    let leading = last_rule + 1;
    if leading * 2 < width
        || kinds[..leading]
            .iter()
            .any(|&kind| kind != BorderCell::Rule)
    {
        return false;
    }
    let [BorderCell::Blank, region @ ..] = &kinds[leading..width - 2] else {
        return false;
    };
    region.first() == Some(&BorderCell::Other)
        && matches!(
            region.last(),
            Some(BorderCell::Other | BorderCell::WideTail)
        )
        && !region
            .windows(2)
            .any(|pair| pair == [BorderCell::Blank, BorderCell::Blank])
}

/// Find the framed input block the cursor is in.
///
/// Walks up from the cursor row to the nearest row whose first cell is the
/// prompt glyph; the row above it must be the box's top border
/// ([`is_input_top_border`]), and the first row below it that is a full-width
/// rule is the block's bottom edge (a label is not allowed for there: it
/// was only ever measured at the top), with the cursor
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
    if prompt_row == 0 || !is_input_top_border(screen, prompt_row - 1, cols) {
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

    // --- a label inside the top border --------------------------------

    fn pane_of_width(cols: u16, bytes: &str) -> Screen {
        let mut screen = Screen::new(ROWS, cols);
        screen.process(bytes.as_bytes());
        screen
    }

    /// The width of `label` in cells for a label of ASCII and CJK ideographs
    /// only (one cell and two cells). A fixture with anything else must give
    /// its width itself, to [`labelled_border_of`], because no rule of thumb
    /// holds for it.
    fn cells(label: &str) -> usize {
        label
            .chars()
            .map(|c| {
                if c.is_ascii() {
                    1
                } else {
                    assert!(
                        ('\u{4e00}'..='\u{9fff}').contains(&c),
                        "{c:?}: give this label's width to labelled_border_of"
                    );
                    2
                }
            })
            .sum()
    }

    /// A top border `cols` wide whose rule carries `label`: rule run, blank,
    /// label in an RGB foreground, blank, one rule cell.
    fn labelled_border(cols: usize, label: &str) -> String {
        labelled_border_of(cols, label, cells(label))
    }

    /// [`labelled_border`] for a label whose width in cells the fixture gives
    /// (the cells the application reserves for it).
    fn labelled_border_of(cols: usize, label: &str, label_cells: usize) -> String {
        let lead = cols - label_cells - 3;
        format!(
            "{} \x1b[38;2;200;120;40m{label}\x1b[0m {RULE_GLYPH}",
            RULE_GLYPH.repeat(lead)
        )
    }

    /// An input block on rows 20-22 (1-based) with `border` as its top row, a
    /// full-width bottom rule, and `prompt` on the prompt row, the cursor
    /// after the text (or at column 2 when it is the bare prompt).
    fn block_with_border(cols: u16, border: &str, prompt: Option<&str>) -> Screen {
        let (body, cursor) = match prompt {
            Some(text) => (text.to_string(), 3 + text.chars().count() as u16),
            None => ("\x1b[2mTry something\x1b[0m".to_string(), 3),
        };
        let frame = format!(
            "\x1b[2J{}{}{}{}",
            at(19, 1, border),
            at(20, 1, &format!("\u{276f}\u{a0}{body}")),
            at(21, 1, &RULE_GLYPH.repeat(cols as usize)),
            at(20, cursor, ""),
        );
        pane_of_width(cols, &frame)
    }

    #[test]
    fn a_label_right_aligned_in_the_top_border_is_read_at_the_widths_measured() {
        for cols in [81u16, 84] {
            let border = labelled_border(cols as usize, "abcdefg");
            assert_eq!(
                read_input_line(&block_with_border(cols, &border, None)),
                InputLine::Empty,
                "{cols}"
            );
            assert_eq!(
                read_input_line(&block_with_border(cols, &border, Some("hello"))),
                InputLine::HasText,
                "{cols}"
            );
        }
    }

    #[test]
    fn a_label_of_several_words_or_wide_characters_is_read_like_the_one_word_label() {
        // Not measured as such (one word of 7 plain cells was): a label's text is free
        // text, so blanks between words and wide characters are allowed for by
        // construction.
        for label in [
            "two words",
            "three little words",
            "twenty ascii chars x",
            "\u{4e16}\u{754c}\u{540d}",
            "abc \u{4e16}\u{754c} def",
            "\u{4e16} \u{754c}",
            "a",
        ] {
            for cols in [81u16, 84] {
                let border = labelled_border(cols as usize, label);
                assert_eq!(
                    read_input_line(&block_with_border(cols, &border, None)),
                    InputLine::Empty,
                    "{label:?} at {cols}"
                );
                assert_eq!(
                    read_input_line(&block_with_border(cols, &border, Some("hello"))),
                    InputLine::HasText,
                    "{label:?} at {cols}"
                );
            }
        }
    }

    #[test]
    fn the_label_is_not_part_of_what_was_typed() {
        for label in ["abcdefg", "two words", "abc \u{4e16}\u{754c} def"] {
            let border = labelled_border(81, label);
            assert_eq!(
                typed_text_without_whitespace(&block_with_border(81, &border, Some("hello")))
                    .as_deref(),
                Some("hello"),
                "{label:?}"
            );
            assert_eq!(
                typed_text_without_whitespace(&block_with_border(81, &border, None)).as_deref(),
                Some(""),
                "{label:?}"
            );
        }
    }

    #[test]
    fn a_longer_label_is_read_up_to_the_half_width_floor_and_not_past_it() {
        // At width 80 the leading rule run must be at least 40 cells, which
        // leaves room for a 37-cell label and no more.
        for (label_len, expected) in [
            (20, InputLine::Empty),
            (37, InputLine::Empty),
            (38, InputLine::Unrecognized),
        ] {
            let border = labelled_border(80, &"n".repeat(label_len));
            assert_eq!(
                read_input_line(&block_with_border(80, &border, None)),
                expected,
                "{label_len}"
            );
        }
    }

    #[test]
    fn the_half_width_boundary_at_an_odd_width_is_exact() {
        // The leading run must be at least half the width, rounded up: 41 of 81
        // (a 37-cell label) and 40 of 79 (a 36-cell label) are the last that fit.
        for (cols, label_len, expected) in [
            (81usize, 37usize, InputLine::Empty),
            (81, 38, InputLine::Unrecognized),
            (79, 36, InputLine::Empty),
            (79, 37, InputLine::Unrecognized),
        ] {
            let border = labelled_border(cols, &"n".repeat(label_len));
            assert_eq!(
                read_input_line(&block_with_border(cols as u16, &border, None)),
                expected,
                "{cols} wide, {label_len}-cell label"
            );
        }
    }

    #[test]
    fn the_labels_content_is_not_examined_so_any_short_text_there_reads_like_the_measured_one() {
        // The accepted consequence, pinned: a hint, a count or a mode word drawn
        // in the same position is the same shape. What would still have to hold
        // for a nudge is the rest of the reader's conditions (prompt glyph at
        // column 0, full-width rule below, cursor in the block) and the member
        // being idle and not waiting, none of which this row decides.
        for (label, label_cells) in [
            ("|", 1),
            ("-", 1),
            ("\u{2502}", 1),
            ("esc to cancel", 13),
            ("12.4k tokens", 12),
            ("plan mode on", 12),
            ("? for shortcuts", 15),
        ] {
            let border = labelled_border_of(80, label, label_cells);
            assert_eq!(
                read_input_line(&block_with_border(80, &border, None)),
                InputLine::Empty,
                "{label:?}"
            );
        }
    }

    #[test]
    fn a_label_the_terminal_lays_out_wider_than_the_application_reserved_is_unrecognized() {
        // The likeliest way a real label brings the old notice back. The
        // application reserves the cells its own width rule gives the label and
        // lays the row out to fit; the terminal model here counts a ZWJ sequence
        // one character at a time (each emoji two cells, the joiner none), so it
        // lays out more cells than were reserved, the row overflows, and the last
        // cell is no longer the single rule cell. It reads Unrecognized: closed,
        // as before the label was allowed for, and no worse.
        let label = "ab \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        // As a grapheme-aware application counts it: "ab " and one two-cell emoji.
        let border = labelled_border_of(80, label, 3 + 2);
        let screen = block_with_border(80, &border, None);
        // Fixture check: the model really did lay it out wider, so the border
        // row no longer ends in its rule cell.
        let last = screen.screen().cell(18, 79).map(|cell| cell.contents());
        assert_ne!(last, Some(RULE_GLYPH), "the row still ends in a rule cell");
        assert_eq!(read_input_line(&screen), InputLine::Unrecognized);
    }

    #[test]
    fn the_half_width_floor_counts_cells_so_a_wide_label_is_capped_sooner() {
        // 18 wide characters are 36 cells (accepted at width 80); 19 are 38.
        for (chars, expected) in [(18, InputLine::Empty), (19, InputLine::Unrecognized)] {
            let border = labelled_border(80, &"\u{4e16}".repeat(chars));
            assert_eq!(
                read_input_line(&block_with_border(80, &border, None)),
                expected,
                "{chars}"
            );
        }
    }

    #[test]
    fn a_label_that_fills_the_border_is_a_row_of_text_and_not_the_input_box() {
        // Rule, blank, 76 cells of label, blank, rule: nothing but the two
        // end cells is border.
        let border = labelled_border(80, &"n".repeat(76));
        assert_eq!(
            read_input_line(&block_with_border(80, &border, None)),
            InputLine::Unrecognized
        );
    }

    #[test]
    fn a_top_border_that_is_not_a_rule_with_one_right_hand_label_is_unrecognized() {
        let rule = |n: usize| RULE_GLYPH.repeat(n);
        // Every row is exactly 80 cells, so each case fails for the reason it
        // names and not because it stops short of the last column.
        let cases: [(&str, String); 14] = [
            (
                "a row of text",
                format!("{:<80}", "just some words on the row above"),
            ),
            ("a blank row", " ".repeat(80)),
            (
                "a dialog title at the left",
                format!("{} Title {}", rule(1), rule(72)),
            ),
            (
                "a title in the middle",
                format!("{} Title {}", rule(36), rule(37)),
            ),
            (
                "mostly blank",
                format!("{}{}{}", rule(1), " ".repeat(78), rule(1)),
            ),
            (
                "a rule, then blanks, then a rule",
                format!("{}{}{}", rule(50), " ".repeat(29), rule(1)),
            ),
            (
                "two label segments",
                format!("{} abc {} def {}", rule(64), rule(5), rule(1)),
            ),
            (
                "a double blank inside the label",
                format!("{} ab  cd {}", rule(71), rule(1)),
            ),
            (
                "two blanks before the label",
                format!("{}  abcdefg {}", rule(69), rule(1)),
            ),
            (
                "two blanks after the label",
                format!("{} abcdefg  {}", rule(69), rule(1)),
            ),
            (
                "a rule cell inside the label",
                format!("{} ab{}cd {}", rule(72), rule(1), rule(1)),
            ),
            (
                "no blank before the label",
                format!("{}abcdefg {}", rule(71), rule(1)),
            ),
            (
                "no rule cell after the label",
                format!("{} abcdefg  ", rule(70)),
            ),
            (
                "two rule cells after the label",
                format!("{} abcdefg {}", rule(69), rule(2)),
            ),
        ];
        for (name, border) in cases {
            assert_eq!(border.chars().count(), 80, "{name}: fixture width");
            assert_eq!(
                read_input_line(&block_with_border(80, &border, None)),
                InputLine::Unrecognized,
                "{name}"
            );
        }
    }

    #[test]
    fn a_label_in_the_bottom_rule_is_not_allowed_for() {
        let frame = format!(
            "\x1b[2J{}{}{}{}",
            at(19, 1, &rule()),
            at(20, 1, "\u{276f}\u{a0}\x1b[2mTry\x1b[0m"),
            at(21, 1, &labelled_border(80, "abcdefg")),
            at(20, 3, ""),
        );
        assert_eq!(read_input_line(&pane(&frame)), InputLine::Unrecognized);
    }

    #[test]
    fn a_label_region_cut_by_a_rule_cell_is_two_regions_and_is_unrecognized() {
        // A word, a rule cell, another word: the second word is a label region
        // of its own and the first is no part of the leading rule.
        let rule = |n: usize| RULE_GLYPH.repeat(n);
        let border = format!("{} abc{}def {}", rule(68), rule(3), rule(1));
        assert_eq!(border.chars().count(), 80);
        assert_eq!(
            read_input_line(&block_with_border(80, &border, None)),
            InputLine::Unrecognized
        );
    }
}
