//! Recognizes SGR mouse escape sequences (`ESC [ < Cb ; Cx ; Cy (M|m)`) that
//! leak through as individual character key events instead of a proper
//! `crossterm::event::Event::Mouse` — observed when banto runs nested inside
//! some terminal multiplexers, where the raw bytes arrive but aren't decoded
//! into a mouse event before reaching us.
//!
//! This module is the pure recognizer only: it matches a buffer of
//! characters accumulated since a leading `ESC` against the SGR grammar,
//! with no knowledge of the event loop or a real terminal, so it can be
//! tested in isolation. [`crate::tui`] owns the buffering loop that feeds it
//! one character at a time and decides what to do with the result.

/// One parsed SGR mouse sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SgrMouseEvent {
    pub button: u32,
    pub x: u16,
    pub y: u16,
    /// `true` for a press (`M` terminator), `false` for a release (`m`).
    pub pressed: bool,
}

/// Result of matching a buffer against the SGR mouse escape grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SgrParse {
    /// The buffer is a valid prefix so far; more characters are needed.
    Incomplete,
    /// The buffer is a complete, valid SGR mouse sequence.
    Complete(SgrMouseEvent),
    /// The buffer can never become a valid sequence.
    NotSgr,
}

/// Longer than any real SGR sequence (button/x/y are far short of this many
/// digits in practice); bounds how long a garbage input can keep the caller
/// buffering instead of ever resolving to [`SgrParse::NotSgr`].
const MAX_LEN: usize = 32;

/// Match `buf` (characters accumulated since a leading `ESC`, `ESC`
/// included) against `ESC [ < Cb ; Cx ; Cy (M|m)`.
pub fn parse_prefix(buf: &[char]) -> SgrParse {
    if buf.len() > MAX_LEN {
        return SgrParse::NotSgr;
    }
    match try_parse(buf, true) {
        Ok(event) => SgrParse::Complete(event),
        Err(incomplete_or_not) => incomplete_or_not,
    }
}

/// Match `buf` (characters accumulated since a leading `[`, the `ESC` byte
/// already missing) against `[ < Cb ; Cx ; Cy (M|m)` — the same grammar as
/// [`parse_prefix`] minus its leading `ESC`. Some delivery paths drop the
/// `ESC` byte entirely before it ever reaches us (confirmed via
/// `BANTO_INPUT_LOG`: real leaked sequences arrive as a headless stream of
/// plain `Char` presses with no preceding `Esc` event), so the caller needs
/// a grammar that doesn't require it.
pub fn parse_headless_prefix(buf: &[char]) -> SgrParse {
    if buf.len() > MAX_LEN {
        return SgrParse::NotSgr;
    }
    match try_parse(buf, false) {
        Ok(event) => SgrParse::Complete(event),
        Err(incomplete_or_not) => incomplete_or_not,
    }
}

fn try_parse(buf: &[char], expect_esc: bool) -> Result<SgrMouseEvent, SgrParse> {
    let mut idx = 0;
    if expect_esc {
        expect_char(buf, &mut idx, '\u{1b}')?;
    }
    expect_char(buf, &mut idx, '[')?;
    expect_char(buf, &mut idx, '<')?;
    let button = read_number(buf, &mut idx)?;
    expect_char(buf, &mut idx, ';')?;
    let x = read_number(buf, &mut idx)?;
    expect_char(buf, &mut idx, ';')?;
    let y = read_number(buf, &mut idx)?;
    let pressed = match buf.get(idx) {
        Some('M') => true,
        Some('m') => false,
        Some(_) => return Err(SgrParse::NotSgr),
        None => return Err(SgrParse::Incomplete),
    };
    let x = u16::try_from(x).map_err(|_| SgrParse::NotSgr)?;
    let y = u16::try_from(y).map_err(|_| SgrParse::NotSgr)?;
    Ok(SgrMouseEvent {
        button,
        x,
        y,
        pressed,
    })
}

/// Require `buf[*idx] == want`, advancing `*idx` past it on success.
fn expect_char(buf: &[char], idx: &mut usize, want: char) -> Result<(), SgrParse> {
    match buf.get(*idx) {
        Some(&c) if c == want => {
            *idx += 1;
            Ok(())
        }
        Some(_) => Err(SgrParse::NotSgr),
        None => Err(SgrParse::Incomplete),
    }
}

/// Read one or more decimal digits starting at `buf[*idx]`, advancing
/// `*idx` past them on success.
fn read_number(buf: &[char], idx: &mut usize) -> Result<u32, SgrParse> {
    let start = *idx;
    while buf.get(*idx).is_some_and(char::is_ascii_digit) {
        *idx += 1;
    }
    if *idx == start {
        return Err(if buf.get(*idx).is_none() {
            SgrParse::Incomplete
        } else {
            SgrParse::NotSgr
        });
    }
    buf[start..*idx]
        .iter()
        .collect::<String>()
        .parse()
        .map_err(|_| SgrParse::NotSgr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn empty_buffer_is_incomplete() {
        assert_eq!(parse_prefix(&[]), SgrParse::Incomplete);
    }

    #[test]
    fn every_valid_prefix_is_incomplete_until_the_last_character() {
        let full = "\u{1b}[<0;10;20M";
        for len in 1..full.chars().count() {
            let prefix: Vec<char> = full.chars().take(len).collect();
            assert_eq!(
                parse_prefix(&prefix),
                SgrParse::Incomplete,
                "expected incomplete at len {len}: {prefix:?}"
            );
        }
    }

    #[test]
    fn complete_press_sequence_parses() {
        let got = parse_prefix(&chars("\u{1b}[<0;10;20M"));
        assert_eq!(
            got,
            SgrParse::Complete(SgrMouseEvent {
                button: 0,
                x: 10,
                y: 20,
                pressed: true,
            })
        );
    }

    #[test]
    fn complete_release_sequence_parses() {
        let got = parse_prefix(&chars("\u{1b}[<2;5;7m"));
        assert_eq!(
            got,
            SgrParse::Complete(SgrMouseEvent {
                button: 2,
                x: 5,
                y: 7,
                pressed: false,
            })
        );
    }

    #[test]
    fn multi_digit_fields_parse() {
        let got = parse_prefix(&chars("\u{1b}[<64;123;456M"));
        assert_eq!(
            got,
            SgrParse::Complete(SgrMouseEvent {
                button: 64,
                x: 123,
                y: 456,
                pressed: true,
            })
        );
    }

    #[test]
    fn wrong_second_character_is_not_sgr() {
        assert_eq!(parse_prefix(&chars("\u{1b}x")), SgrParse::NotSgr);
    }

    #[test]
    fn wrong_third_character_is_not_sgr() {
        assert_eq!(parse_prefix(&chars("\u{1b}[x")), SgrParse::NotSgr);
    }

    #[test]
    fn non_digit_where_a_number_is_expected_is_not_sgr() {
        assert_eq!(parse_prefix(&chars("\u{1b}[<a")), SgrParse::NotSgr);
    }

    #[test]
    fn missing_semicolon_after_a_number_is_not_sgr() {
        assert_eq!(parse_prefix(&chars("\u{1b}[<0x")), SgrParse::NotSgr);
    }

    #[test]
    fn invalid_terminator_is_not_sgr() {
        assert_eq!(parse_prefix(&chars("\u{1b}[<0;1;2X")), SgrParse::NotSgr);
    }

    #[test]
    fn coordinates_beyond_u16_are_not_sgr() {
        assert_eq!(parse_prefix(&chars("\u{1b}[<0;70000;1M")), SgrParse::NotSgr);
    }

    #[test]
    fn overlong_garbage_terminates_as_not_sgr_instead_of_buffering_forever() {
        let garbage: String = "\u{1b}[<"
            .chars()
            .chain(std::iter::repeat_n('9', 40))
            .collect();
        assert_eq!(parse_prefix(&chars(&garbage)), SgrParse::NotSgr);
    }

    #[test]
    fn a_lone_esc_is_incomplete_not_not_sgr() {
        // Distinguishing this matters: it's what lets the caller wait
        // briefly for a possible follow-up instead of assuming garbage.
        assert_eq!(parse_prefix(&chars("\u{1b}")), SgrParse::Incomplete);
    }

    #[test]
    fn plain_text_after_esc_is_not_sgr() {
        // e.g. the user pressed Esc then started typing "hello" very fast.
        assert_eq!(parse_prefix(&chars("\u{1b}hello")), SgrParse::NotSgr);
    }

    /// The exact shape confirmed by `BANTO_INPUT_LOG`: a leaked sequence
    /// with its leading `ESC` byte already missing.
    #[test]
    fn headless_complete_sequence_parses_without_a_leading_esc() {
        let got = parse_headless_prefix(&chars("[<35;18;12M"));
        assert_eq!(
            got,
            SgrParse::Complete(SgrMouseEvent {
                button: 35,
                x: 18,
                y: 12,
                pressed: true,
            })
        );
    }

    #[test]
    fn headless_every_valid_prefix_is_incomplete_until_the_last_character() {
        let full = "[<0;10;20M";
        for len in 1..full.chars().count() {
            let prefix: Vec<char> = full.chars().take(len).collect();
            assert_eq!(
                parse_headless_prefix(&prefix),
                SgrParse::Incomplete,
                "expected incomplete at len {len}: {prefix:?}"
            );
        }
    }

    #[test]
    fn headless_plain_bracket_then_non_matching_char_is_not_sgr() {
        // e.g. the user genuinely typed "[x" into the search box.
        assert_eq!(parse_headless_prefix(&chars("[x")), SgrParse::NotSgr);
    }

    #[test]
    fn headless_a_lone_bracket_is_incomplete_not_not_sgr() {
        assert_eq!(parse_headless_prefix(&chars("[")), SgrParse::Incomplete);
    }
}
