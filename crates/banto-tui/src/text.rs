//! Column-aware text truncation: cutting a string to fit a display-column
//! budget without splitting a wide character mid-glyph. Shared by
//! `render_modal` (dialog prompts, candidate lists) and `view` (the session
//! list's title/cwd truncation) — a rendering concern, not a domain one, so
//! it lives in `banto-tui` rather than `banto-core`.

#[cfg(test)]
use banto_core::display_text::MAX_ZERO_WIDTH_RUN;
use banto_core::display_text::cap_zero_width_runs;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate `s` to fit within `max_width` display columns (a full-width
/// character — e.g. Japanese — counts as 2, matching how a terminal actually
/// advances the cursor for it), appending a trailing ellipsis when anything
/// was cut. `ratatui` already clips a `Paragraph`/`ListItem` cleanly at its
/// own area boundary, so this isn't papering over a rendering bug — it's so
/// long content (a session title, a cwd, a group name) that gets cut ends in
/// a visible `…` instead of silently vanishing past the edge with no
/// indication anything was hidden. A run of zero-width characters is cut to
/// [`MAX_ZERO_WIDTH_RUN`] whether or not the text needed shortening.
pub fn truncate_to_width(s: &str, max_width: u16) -> String {
    let s = cap_zero_width_runs(s);
    let s = s.as_ref();
    let max_width = max_width as usize;
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width - 1; // reserve 1 column for the ellipsis
    let mut out = String::new();
    let mut width = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if width + w > budget {
            break;
        }
        out.push(c);
        width += w;
    }
    out.push('\u{2026}');
    out
}

/// The mirror of [`truncate_to_width`]: truncate `s` from the LEFT to fit
/// within `max_width` display columns, prefixing a leading ellipsis when
/// anything was cut. For content where the *tail* carries the information —
/// a cwd path, where "…project/src/main.rs" is more useful than
/// "/home/user/deeply/nested/…". Zero-width runs are cut as in
/// [`truncate_to_width`].
pub fn truncate_to_width_leading(s: &str, max_width: u16) -> String {
    let s = cap_zero_width_runs(s);
    let s = s.as_ref();
    let max_width = max_width as usize;
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width - 1; // reserve 1 column for the ellipsis
    let mut kept: Vec<char> = Vec::new();
    let mut width = 0usize;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if width + w > budget {
            break;
        }
        kept.push(c);
        width += w;
    }
    kept.reverse();
    let mut out = String::with_capacity(kept.len() + 1);
    out.push('\u{2026}');
    out.extend(kept);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_to_width_leaves_short_text_untouched() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_width_cuts_ascii_and_appends_an_ellipsis() {
        assert_eq!(truncate_to_width("hello world", 6), "hello\u{2026}");
    }

    #[test]
    fn truncate_to_width_never_splits_a_full_width_character() {
        // Each "あ" is 2 display columns; the budget for content is
        // max_width - 1 (reserved for the ellipsis) = 4, which fits exactly
        // 2 of them (4 columns) with none left over for a 3rd.
        assert_eq!(truncate_to_width(&"あ".repeat(5), 5), "ああ\u{2026}");
    }

    #[test]
    fn truncate_to_width_at_zero_is_empty() {
        assert_eq!(truncate_to_width("hello", 0), "");
    }

    #[test]
    fn truncate_to_width_leading_leaves_short_text_untouched() {
        assert_eq!(truncate_to_width_leading("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_width_leading_cuts_from_the_front_and_prepends_an_ellipsis() {
        // budget = max_width - 1 = 14 columns of content (each char here is
        // 1 column), so exactly the last 14 characters survive.
        assert_eq!(
            truncate_to_width_leading("/home/user/project/src/main.rs", 15),
            "\u{2026}ct/src/main.rs"
        );
    }

    #[test]
    fn truncate_to_width_leading_never_splits_a_full_width_character() {
        // Same budget math as the trailing variant, mirrored: 4 content
        // columns fit exactly 2 "あ" (4 columns), taken from the tail.
        let s = format!("{}{}", "い".repeat(3), "あ".repeat(2));
        assert_eq!(truncate_to_width_leading(&s, 5), "\u{2026}ああ");
    }

    #[test]
    fn truncate_to_width_leading_at_zero_is_empty() {
        assert_eq!(truncate_to_width_leading("hello", 0), "");
    }

    #[test]
    fn truncate_to_width_leading_handles_japanese_titles() {
        // A Japanese cwd tail, longer than the budget: keep the tail intact.
        let path = "/repo/日本語プロジェクト/src";
        let truncated = truncate_to_width_leading(path, 10);
        assert!(truncated.starts_with('\u{2026}'));
        assert!(truncated.width() <= 10);
        assert!(path.ends_with(truncated.trim_start_matches('\u{2026}')));
    }

    #[test]
    fn a_run_of_combining_marks_is_cut_even_when_the_text_fits() {
        // One letter and a thousand combining acutes is one column wide, so it
        // fits any budget and used to come through whole.
        let text = format!("e{}", "\u{0301}".repeat(1000));
        for out in [
            truncate_to_width(&text, 10),
            truncate_to_width_leading(&text, 10),
        ] {
            assert_eq!(out.chars().count(), 1 + MAX_ZERO_WIDTH_RUN);
            assert!(out.starts_with('e'));
        }
    }

    #[test]
    fn marks_after_the_last_letter_that_fits_stop_at_the_cap() {
        // The budget is spent on the letters; the marks after the last one kept
        // cost no column, so only the cap bounds them.
        let text = format!("abc{}def", "\u{0301}".repeat(1000));
        let out = truncate_to_width(&text, 4);
        assert_eq!(out.chars().count(), 3 + MAX_ZERO_WIDTH_RUN + 1);
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn a_run_within_the_cap_is_left_alone() {
        // A Thai letter with its vowel and tone mark, a decomposed Vietnamese
        // letter with two marks, an emoji with its joiner and selector: text.
        for text in [
            "\u{0e01}\u{0e34}\u{0e48}",
            "e\u{0302}\u{0301}",
            "\u{1f469}\u{200d}\u{1f4bb}\u{fe0f}",
        ] {
            assert_eq!(truncate_to_width(text, 20), text);
            assert_eq!(truncate_to_width_leading(text, 20), text);
        }
    }

    #[test]
    fn marks_separated_by_letters_are_each_counted_from_zero() {
        let text = format!("{0}a{0}b", "\u{0301}".repeat(MAX_ZERO_WIDTH_RUN));
        assert_eq!(truncate_to_width(&text, 10), text);
    }
}
