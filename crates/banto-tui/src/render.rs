//! Render a `vt100` screen into ratatui text (one `Line` per screen row),
//! preserving colors and the common attributes Claude Code uses.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

/// Convert the current terminal screen into ratatui text sized to the screen.
pub fn screen_to_text(screen: &vt100::Screen) -> Text<'static> {
    let (rows, cols) = screen.size();
    let mut lines = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols as usize);
        for col in 0..cols {
            match screen.cell(row, col) {
                Some(cell) => {
                    if cell.is_wide_continuation() {
                        continue; // the wide glyph's left cell already spans 2 cols
                    }
                    let content = if cell.has_contents() {
                        cell.contents().to_string()
                    } else {
                        " ".to_string()
                    };
                    let mut style = Style::default()
                        .fg(conv_color(cell.fgcolor()))
                        .bg(conv_color(cell.bgcolor()));
                    if cell.bold() {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if cell.dim() {
                        style = style.add_modifier(Modifier::DIM);
                    }
                    if cell.italic() {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if cell.underline() {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    if cell.inverse() {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    spans.push(Span::styled(content, style));
                }
                None => spans.push(Span::raw(" ")),
            }
        }
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

fn conv_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::{conv_color, screen_to_text};

    #[test]
    fn maps_vt100_colors() {
        assert_eq!(conv_color(vt100::Color::Default), Color::Reset);
        assert_eq!(conv_color(vt100::Color::Idx(4)), Color::Indexed(4));
        assert_eq!(conv_color(vt100::Color::Rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
    }

    #[test]
    fn renders_one_line_per_row_with_contents() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"hi");
        let text = screen_to_text(parser.screen());
        assert_eq!(text.lines.len(), 3);
        let first: String = text.lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(first.starts_with("hi"), "got {first:?}");
    }

    #[test]
    fn sgr_2_dim_survives_vt100_and_maps_to_the_dim_modifier() {
        // Claude Code's inline ghost/autosuggest text is rendered with SGR 2
        // (faint) in a naked terminal; vt100 0.16 models this on `Cell::dim`
        // (see `attrs.rs`'s `set_dim`/`TEXT_MODE_DIM`), so it must survive the
        // vt100 -> ratatui conversion rather than being dropped on the floor.
        let mut parser = vt100::Parser::new(1, 10, 0);
        parser.process(b"\x1b[2mdim\x1b[0mnormal");
        let text = screen_to_text(parser.screen());
        let spans = &text.lines[0].spans;

        let dim_span = spans.iter().find(|s| s.content == "d").unwrap();
        assert!(dim_span.style.add_modifier.contains(Modifier::DIM));

        let normal_span = spans.iter().find(|s| s.content == "n").unwrap();
        assert!(!normal_span.style.add_modifier.contains(Modifier::DIM));
    }

    // --- vt100 -> ratatui column-width contract (audit) --------------------
    //
    // `screen_to_text` pushes one `Span` per non-wide-continuation vt100
    // cell. That only lines up on screen if, for every such cell, ratatui's
    // own width measurement of `cell.contents()` agrees with how many
    // columns vt100 itself decided that cell occupies — if the two disagree
    // for some glyph, `Buffer::set_line`'s per-span x-advance
    // (`buffer.rs`'s `set_stringn`) drifts out of step with vt100's column
    // grid, and every span after it in the row paints at the wrong place.
    //
    // These tests check that agreement directly: parse a glyph through a
    // real `vt100::Parser`, type a marker character right after it, and
    // compare which column vt100 put the marker in against which column it
    // actually landed in once the same screen goes through `screen_to_text`
    // and a `ratatui::widgets::Paragraph`.
    mod width_contract {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::{Paragraph, Widget};

        use super::screen_to_text;

        const COLS: u16 = 20;

        /// The column vt100 placed a `Z` marker in, typed immediately after
        /// `input` — i.e. how many grid columns vt100 believes `input`
        /// occupied.
        fn vt100_marker_column(input: &[u8]) -> u16 {
            let mut parser = vt100::Parser::new(1, COLS, 0);
            parser.process(input);
            parser.process(b"Z");
            let screen = parser.screen();
            for col in 0..COLS {
                if let Some(cell) = screen.cell(0, col)
                    && cell.contents() == "Z"
                {
                    return col;
                }
            }
            panic!("marker Z not found in vt100 screen for {input:?}");
        }

        /// The column ratatui actually painted the same `Z` marker into,
        /// after round-tripping the same vt100 screen through
        /// `screen_to_text` and a `Paragraph` — no `Terminal`, no real
        /// backend: `Buffer::empty` + `Widget::render` is enough to observe
        /// where content lands.
        fn ratatui_marker_column(input: &[u8]) -> u16 {
            let mut parser = vt100::Parser::new(1, COLS, 0);
            parser.process(input);
            parser.process(b"Z");
            let text = screen_to_text(parser.screen());
            let area = Rect::new(0, 0, COLS, 1);
            let mut buffer = Buffer::empty(area);
            Paragraph::new(text).render(area, &mut buffer);
            for col in 0..COLS {
                if buffer[(col, 0)].symbol() == "Z" {
                    return col;
                }
            }
            panic!("marker Z not found in ratatui buffer for {input:?}");
        }

        /// Assert vt100 and ratatui agree on how many columns `input`
        /// occupies. A failure here is the contract breaking: `input`
        /// itself painted correctly (or not) is secondary to whether
        /// everything typed *after* it in the same row lands where vt100
        /// says it should.
        fn assert_widths_agree(name: &str, input: &[u8]) {
            let vt100_col = vt100_marker_column(input);
            let ratatui_col = ratatui_marker_column(input);
            assert_eq!(
                vt100_col, ratatui_col,
                "{name}: vt100 placed the marker at column {vt100_col}, \
                 ratatui painted it at column {ratatui_col}"
            );
        }

        #[test]
        fn plain_emoji_name_badge_agrees() {
            assert_widths_agree("U+1F4DB (name badge)", "\u{1F4DB}".as_bytes());
        }

        #[test]
        fn plain_emoji_file_folder_agrees() {
            assert_widths_agree("U+1F4C1 (file folder)", "\u{1F4C1}".as_bytes());
        }

        #[test]
        fn eight_spoked_asterisk_plus_vs16_agrees() {
            assert_widths_agree(
                "U+2733 U+FE0F (eight spoked asterisk + VS16)",
                "\u{2733}\u{FE0F}".as_bytes(),
            );
        }

        #[test]
        fn eight_spoked_asterisk_alone_agrees() {
            assert_widths_agree("U+2733 alone, no VS16", "\u{2733}".as_bytes());
        }

        #[test]
        fn nerd_font_pua_glyph_agrees() {
            assert_widths_agree("U+E0A0 (nerd font PUA)", "\u{E0A0}".as_bytes());
        }

        #[test]
        fn full_block_agrees() {
            assert_widths_agree("U+2588 (full block)", "\u{2588}".as_bytes());
        }

        #[test]
        fn light_shade_agrees() {
            assert_widths_agree("U+2591 (light shade)", "\u{2591}".as_bytes());
        }

        #[test]
        fn black_right_pointing_triangle_alone_agrees() {
            assert_widths_agree("U+25B6 alone (text presentation)", "\u{25B6}".as_bytes());
        }

        #[test]
        fn black_right_pointing_triangle_plus_vs16_agrees() {
            assert_widths_agree(
                "U+25B6 U+FE0F (triangle + VS16, emoji presentation)",
                "\u{25B6}\u{FE0F}".as_bytes(),
            );
        }

        #[test]
        fn cjk_run_agrees() {
            assert_widths_agree(
                "banto\u{958B}\u{767A} (CJK run)",
                "banto\u{958B}\u{767A}".as_bytes(),
            );
        }
    }
}
