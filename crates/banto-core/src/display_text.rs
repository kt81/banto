//! The one policy for text that someone else wrote and banto puts on screen
//! inside a line of its own: a session title, a brigade's name, the first line
//! of a message. Pure.
//!
//! The terminal treats what is written to it as instructions as well as
//! characters, and a title is whatever the agent or the provider stored — the
//! providers fold its whitespace and nothing more. So every such text is
//! reduced here, once, before it is cut to width or interpolated into a status
//! or a strip, and nowhere else decides what is "safe".

use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The most zero-width characters (combining marks, joiners, variation
/// selectors) kept in a row. A zero-width character costs no column, so neither
/// a width test nor a budget ever stops a run of them: a title or a name with
/// thousands of marks after one letter came through whole, as one cell's
/// symbol. Real text stacks a few at most (a Thai vowel and a tone mark, Hebrew
/// points with a cantillation mark, an emoji's joiner and selector), so the cap
/// sits well above that and only cuts what is not text.
///
/// Held here, with [`cap_zero_width_runs`], for every path that puts someone
/// else's text on screen: [`display_safe`] and [`shorten_to_columns`] apply it
/// here, and `banto_tui::text`'s two column-cutting helpers call the same
/// function (the dependency runs from the tui to the core, not back).
pub const MAX_ZERO_WIDTH_RUN: usize = 8;

/// `s` with every run of zero-width characters cut to [`MAX_ZERO_WIDTH_RUN`];
/// borrowed when none is longer.
pub fn cap_zero_width_runs(s: &str) -> Cow<'_, str> {
    let mut run = 0usize;
    let mut too_long = false;
    for c in s.chars() {
        if c.width().unwrap_or(0) == 0 {
            run += 1;
            if run > MAX_ZERO_WIDTH_RUN {
                too_long = true;
                break;
            }
        } else {
            run = 0;
        }
    }
    if !too_long {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut run = 0usize;
    for c in s.chars() {
        if c.width().unwrap_or(0) == 0 {
            run += 1;
            if run > MAX_ZERO_WIDTH_RUN {
                continue;
            }
        } else {
            run = 0;
        }
        out.push(c);
    }
    Cow::Owned(out)
}

/// Whether `c` is a Unicode *format* character (general category Cf):
/// invisible characters that change how neighbouring text is laid out or
/// padded rather than drawing anything — bidirectional overrides and isolates
/// that reorder what is shown, zero-width spaces and joiners, the BOM, soft
/// hyphen, tag characters.
///
/// An explicit table rather than a classifier because nothing already in the
/// dependency tree exposes general categories (`char` has `is_control`, which
/// is Cc only; `unicode-width` and `unicode-segmentation` answer other
/// questions), and a dependency for one table is not worth it. The ranges are
/// the Cf entries of `UnicodeData.txt` as of Unicode 16.0. A later Unicode
/// version can add a few; the failure is then one stray invisible character,
/// not a way to steer the terminal (control characters are filtered by
/// `char::is_control`, independently). Characters that are invisible but not
/// Cf — the Hangul and braille blanks, for one — are deliberately left alone.
pub fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// `text` without anything that could steer the terminal or the layout:
/// control characters (the escape that opens a sequence, the C1 range, a
/// carriage return) and format characters (bidi overrides and isolates,
/// zero-width spaces and joiners) are dropped, a tab becomes a space. Nothing
/// else is touched — no trimming, no folding of whitespace — but a run of
/// zero-width characters is cut to [`MAX_ZERO_WIDTH_RUN`].
///
/// A zero-width joiner is dropped like the rest, so an emoji sequence the
/// joiner held together shows as its parts: the price of one rule with no
/// exception for the one Cf character that is sometimes meaningful.
pub fn display_safe(text: &str) -> String {
    let kept: String = text
        .chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            c if c.is_control() || is_format_char(c) => None,
            c => Some(c),
        })
        .collect();
    // After the drops: a joiner or a control character between two marks was
    // not a break in the run.
    match cap_zero_width_runs(&kept) {
        Cow::Borrowed(_) => kept,
        Cow::Owned(capped) => capped,
    }
}

/// `text` as a one-line name: every whitespace run (a newline included)
/// becomes one space, and what remains goes through [`display_safe`]. `None`
/// when nothing visible is left, so the caller falls through to its next
/// source rather than naming something with an empty string.
///
/// Whitespace is folded *before* the characters are dropped: a newline is a
/// control character, and dropping it first would glue two lines into one
/// word.
pub fn display_name(text: &str) -> Option<String> {
    let name = text
        .split_whitespace()
        .map(display_safe)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!name.is_empty()).then_some(name)
}

/// `text` cut to at most `columns` display columns, ending in `…` when
/// anything was cut. By display width, not characters: a wide (Japanese)
/// character takes two columns. By grapheme cluster, not characters: a base
/// letter and the combining mark after it, a flag's two regional indicators,
/// an emoji and its skin-tone modifier are kept or cut together — cutting
/// half a flag. A run of zero-width characters is cut to [`MAX_ZERO_WIDTH_RUN`]
/// whether or not the text needed shortening.
pub fn shorten_to_columns(text: &str, columns: usize) -> String {
    let text = cap_zero_width_runs(text);
    let text = text.as_ref();
    if text.width() <= columns {
        return text.to_string();
    }
    // One column is the ellipsis's.
    let budget = columns.saturating_sub(1);
    let mut kept = String::new();
    let mut width = 0;
    for cluster in text.graphemes(true) {
        let cell = cluster.width();
        if width + cell > budget {
            break;
        }
        kept.push_str(cluster);
        width += cell;
    }
    kept.push('\u{2026}');
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_and_escape_sequences_are_dropped() {
        // The ESC that opens the sequence goes; the printable tail is just text.
        assert_eq!(display_safe("a\u{1b}[31mred\u{1b}[0m"), "a[31mred[0m");
        // An OSC that would set the window title or open a link.
        assert_eq!(
            display_safe("x\u{1b}]8;;http://example.invalid\u{7}y"),
            "x]8;;http://example.invalidy"
        );
        // C0 and C1 controls, DEL, NEL.
        assert_eq!(display_safe("a\u{0}b\u{8}c\u{7f}d\u{9b}e\u{85}f"), "abcdef");
        // A tab is a space; a carriage return is gone.
        assert_eq!(display_safe("a\tb\rc"), "a bc");
    }

    #[test]
    fn a_name_is_one_line_with_whitespace_folded_and_controls_dropped() {
        assert_eq!(
            display_name("  Fix \n the\t\u{1b}[2Jrelay \u{85} guard  ").as_deref(),
            Some("Fix the [2Jrelay guard")
        );
        // Two lines are two words, not one.
        assert_eq!(display_name("one\ntwo").as_deref(), Some("one two"));
        // A word that was only invisible characters leaves no double space.
        assert_eq!(display_name("a \u{200b} b").as_deref(), Some("a b"));
    }

    #[test]
    fn bidi_overrides_and_isolates_are_dropped() {
        assert_eq!(
            display_name("safe\u{202e}txet\u{202c} \u{2066}iso\u{2069}").as_deref(),
            Some("safetxet iso")
        );
        for c in [
            '\u{202a}', '\u{202d}', '\u{2067}', '\u{2068}', '\u{200f}', '\u{61c}',
        ] {
            assert_eq!(display_safe(&format!("a{c}b")), "ab", "{c:?}");
        }
    }

    #[test]
    fn a_zero_width_joiner_inside_a_word_is_dropped() {
        assert_eq!(display_name("re\u{200d}lay").as_deref(), Some("relay"));
        assert_eq!(
            display_name("re\u{200b}lay\u{feff}").as_deref(),
            Some("relay")
        );
    }

    #[test]
    fn a_name_that_reduces_to_nothing_is_absent() {
        assert_eq!(display_name(""), None);
        assert_eq!(display_name("  \t\n "), None);
        assert_eq!(display_name("\u{1b}\u{200b}\u{202e}"), None);
        assert_eq!(display_name(" \u{1b} \u{2066} "), None);
    }

    #[test]
    fn the_format_character_table_covers_its_range_ends_and_nothing_just_outside() {
        for (first, last) in [
            ('\u{200B}', '\u{200F}'),
            ('\u{202A}', '\u{202E}'),
            ('\u{2060}', '\u{2064}'),
            ('\u{2066}', '\u{206F}'),
            ('\u{E0020}', '\u{E007F}'),
        ] {
            assert!(
                is_format_char(first) && is_format_char(last),
                "{first:?}..{last:?}"
            );
            let before = char::from_u32(first as u32 - 1).unwrap();
            let after = char::from_u32(last as u32 + 1).unwrap();
            assert!(!is_format_char(before), "{before:?}");
            assert!(!is_format_char(after), "{after:?}");
        }
        for c in ['\u{AD}', '\u{FEFF}', '\u{180E}', '\u{61C}'] {
            assert!(is_format_char(c), "{c:?}");
        }
        // Not format characters: letters, digits, a no-break space, the
        // Hangul filler (Lo) and the braille blank (So) are left alone.
        for c in ['a', '7', '\u{a0}', '\u{3164}', '\u{2800}', '\u{3042}'] {
            assert!(!is_format_char(c), "{c:?}");
        }
    }

    #[test]
    fn a_cut_is_by_display_width_and_ends_in_an_ellipsis() {
        assert_eq!(shorten_to_columns("short", 10), "short");
        assert_eq!(shorten_to_columns(&"a".repeat(10), 10), "a".repeat(10));
        assert_eq!(
            shorten_to_columns(&"a".repeat(11), 10),
            format!("{}…", "a".repeat(9))
        );
        // Wide characters are two columns each and never split.
        let cut = shorten_to_columns(&"\u{30c6}".repeat(20), 10);
        assert_eq!(cut, format!("{}…", "\u{30c6}".repeat(4)));
        assert_eq!(cut.width(), 9);
    }

    #[test]
    fn a_cut_never_separates_a_combining_mark_from_its_letter() {
        // "e" + U+0301 (acute), repeated: each cluster is one column. Budget
        // for 5 columns is 4 clusters, every mark still on its letter.
        let text = "e\u{301}".repeat(10);
        let cut = shorten_to_columns(&text, 5);
        assert_eq!(cut, format!("{}…", "e\u{301}".repeat(4)));
        // The cut falls exactly after a letter that a mark follows: the mark
        // is kept with it, not stranded at the head of the dropped part.
        let text = format!("abcd{}", "e\u{301}".repeat(10));
        let cut = shorten_to_columns(&text, 6);
        assert_eq!(cut, "abcde\u{301}\u{2026}");
        assert!(!cut.starts_with('\u{301}'));
    }

    #[test]
    fn a_cut_never_splits_an_emoji_sequence_or_a_flag() {
        // The joiner is dropped by `display_safe` before any cut, so what
        // reaches here is the parts: each emoji is its own cluster.
        let family = display_safe("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}");
        assert_eq!(family, "\u{1f468}\u{1f469}\u{1f467}");
        let cut = shorten_to_columns(&family.repeat(4), 7);
        assert!(cut.ends_with('\u{2026}'));
        assert_eq!(cut.chars().filter(|c| *c == '\u{1f467}').count(), 1);
        assert!(cut.width() <= 7, "{cut:?}");

        // Text that does carry a joiner (a caller that did not reduce it first)
        // still cuts between clusters, never inside the sequence.
        let joined = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}".repeat(3);
        for columns in 3..12 {
            let cut = shorten_to_columns(&joined, columns);
            let body = cut.strip_suffix('\u{2026}').unwrap_or(&cut);
            assert!(
                body.is_empty() || body.graphemes(true).all(|g| g.chars().count() % 5 == 0),
                "columns {columns}: {cut:?}"
            );
            assert!(cut.width() <= columns, "{cut:?}");
        }

        // A flag is two regional indicators: kept whole or dropped whole.
        let flags = "\u{1f1ef}\u{1f1f5}".repeat(6);
        for columns in 3..12 {
            let cut = shorten_to_columns(&flags, columns);
            let body = cut.strip_suffix('\u{2026}').unwrap_or(&cut);
            assert_eq!(body.chars().count() % 2, 0, "columns {columns}: {cut:?}");
            assert!(cut.width() <= columns, "{cut:?}");
        }
        // A heart with its emoji-presentation selector is two columns wide as a
        // whole but one by its characters' widths added up; the cut counts the
        // whole, so a row of them never overflows the width it was cut to.
        let hearts = "\u{2764}\u{fe0f}".repeat(8);
        for columns in 3..12 {
            let cut = shorten_to_columns(&hearts, columns);
            assert!(cut.width() <= columns, "columns {columns}: {cut:?}");
        }
        // An emoji and its skin-tone modifier stay together.
        let thumbs = "\u{1f44d}\u{1f3fd}".repeat(6);
        for columns in 3..12 {
            let cut = shorten_to_columns(&thumbs, columns);
            let body = cut.strip_suffix('\u{2026}').unwrap_or(&cut);
            assert_eq!(body.chars().count() % 2, 0, "columns {columns}: {cut:?}");
            assert!(cut.width() <= columns, "{cut:?}");
        }
    }

    #[test]
    fn a_run_of_combining_marks_is_cut_in_a_name_and_in_a_shortened_text() {
        // One letter and a thousand combining acutes is one column wide: it fits
        // any budget, and the width test and the cut both used to let it through.
        let text = format!("e{}", "\u{0301}".repeat(1000));
        let expected = 1 + MAX_ZERO_WIDTH_RUN;
        assert_eq!(shorten_to_columns(&text, 60).chars().count(), expected);
        assert_eq!(display_safe(&text).chars().count(), expected);
        assert_eq!(
            display_name(&text)
                .expect("a letter is visible")
                .chars()
                .count(),
            expected
        );
        // Also when the text is cut: the marks after the last letter that fits.
        let long = format!("abc{}def", "\u{0301}".repeat(1000));
        let cut = shorten_to_columns(&long, 4);
        assert!(cut.chars().count() <= 3 + MAX_ZERO_WIDTH_RUN + 1, "{cut:?}");
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn a_run_within_the_cap_is_left_alone_and_each_run_counts_from_zero() {
        for text in [
            "\u{0e01}\u{0e34}\u{0e48}",
            "e\u{0302}\u{0301}",
            "\u{1f469}\u{1f4bb}\u{fe0f}",
        ] {
            assert_eq!(shorten_to_columns(text, 20), text);
            assert_eq!(display_safe(text), text);
        }
        let text = format!("{0}a{0}b", "\u{0301}".repeat(MAX_ZERO_WIDTH_RUN));
        assert_eq!(shorten_to_columns(&text, 10), text);
        assert_eq!(cap_zero_width_runs(&text), text);
    }
}
