//! Paint a `vt100` screen into a ratatui buffer, one grid cell to one buffer
//! cell.
//!
//! [`render::screen_to_text`](crate::render) built a `Span` per grid cell and
//! let ratatui measure each one independently. That put two width authorities
//! in the same path, and they disagreed: an emoji-presentation sequence was
//! one column to `vt100` and two to ratatui, so every column after one drifted
//! and the child's repaints landed on the wrong cells. Here the column count
//! comes from `vt100` and is handed to ratatui as
//! [`CellDiffOption::ForcedWidth`], so there is no second opinion left to
//! disagree with — the contract `render`'s `width_contract` tests assert is
//! structural here rather than coincidental.
//!
//! Painting cells directly is also the only way to carry OSC 8 hyperlinks.
//! ratatui supports them by placing the escape inside a cell's own symbol
//! (`ratatui-core`'s `merge_diff_link` test), which a `Span` cannot express:
//! its grapheme iteration scatters the sequence, dropping the ESC as
//! zero-width and painting `]8;;http://...` as visible text.

use std::num::NonZeroU16;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// `ESC ]8;;` — opens and closes an OSC 8 hyperlink; the closing form carries
/// an empty URI.
const OSC8: &str = "\u{1b}]8;;";
/// `ESC \` — the string terminator. BEL also terminates an OSC, but ST is what
/// every terminal accepts and what the children banto hosts emit.
const ST: &str = "\u{1b}\u{5c}";

/// Paint `screen`'s visible grid into `area` of `buf`.
///
/// Rows and columns beyond `area` are dropped rather than wrapped: the caller
/// sized the pane, and a grid that outgrew it is mid-resize.
pub fn paint_screen(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) {
    let (rows, cols) = screen.size();
    for row in 0..rows.min(area.height) {
        let mut open_link = 0u32;
        let mut last_painted: Option<(u16, u16)> = None;

        for col in 0..cols.min(area.width) {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            // The wide glyph to its left already claimed this column, and
            // ratatui requires the trailing cell to be blank (`Buffer::diff`:
            // "no double-width cell is followed by a non-blank cell").
            if cell.is_wide_continuation() {
                buf[(area.x + col, area.y + row)].reset();
                continue;
            }

            let (x, y) = (area.x + col, area.y + row);
            let width = if cell.is_wide() { 2 } else { 1 };
            let mut symbol: String = if cell.has_contents() {
                cell.contents().to_string()
            } else {
                " ".to_string()
            };

            let link = cell.hyperlink_id();
            if link != open_link {
                if let Some(prev) = last_painted.take_if(|_| open_link != 0) {
                    close_link_at(buf, prev);
                }
                if link != 0
                    && let Some(uri) = screen.hyperlink_uri(link)
                {
                    symbol.insert_str(0, &format!("{OSC8}{uri}{ST}"));
                }
                open_link = link;
            }

            let painted = &mut buf[(x, y)];
            painted.set_symbol(&symbol);
            painted.set_style(cell_style(cell));
            // Always forced, never inferred: the symbol may carry an escape
            // sequence, whose measured width is meaningless.
            painted.set_diff_option(CellDiffOption::ForcedWidth(
                NonZeroU16::new(width).expect("1 or 2"),
            ));
            last_painted = Some((x, y));
        }

        // A link running to the right edge still has to be closed, or the
        // terminal keeps every following row hyperlinked.
        if open_link != 0
            && let Some(prev) = last_painted
        {
            close_link_at(buf, prev);
        }
    }
}

fn close_link_at(buf: &mut Buffer, (x, y): (u16, u16)) {
    let closed = format!("{}{OSC8}{ST}", buf[(x, y)].symbol());
    buf[(x, y)].set_symbol(&closed);
}

fn cell_style(cell: &vt100::Cell) -> Style {
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
    style
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
    use super::*;

    fn painted(bytes: &[u8], cols: u16) -> (Buffer, vt100::Parser) {
        let mut parser = vt100::Parser::new(1, cols, 0);
        parser.process(bytes);
        let area = Rect::new(0, 0, cols, 1);
        let mut buf = Buffer::empty(area);
        paint_screen(parser.screen(), area, &mut buf);
        (buf, parser)
    }

    /// The column a marker character lands in is the whole width contract:
    /// it must be the column `vt100` put it in, for every glyph class.
    fn marker_column(text: &str) -> u16 {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'Z');
        let (buf, _) = painted(&bytes, 20);
        (0..20)
            .find(|&c| buf[(c, 0)].symbol() == "Z")
            .expect("marker painted")
    }

    #[test]
    fn every_glyph_class_lands_where_vt100_put_it() {
        assert_eq!(marker_column("ab"), 2, "narrow");
        assert_eq!(marker_column("\u{1F4DB}"), 2, "emoji, wide on its own");
        assert_eq!(marker_column("\u{2733}\u{FE0F}"), 2, "emoji + VS16");
        assert_eq!(marker_column("\u{25B6}\u{FE0F}"), 2, "triangle + VS16");
        assert_eq!(marker_column("\u{2733}"), 1, "same base without VS16");
        assert_eq!(marker_column("\u{E0A0}"), 1, "private use area");
        assert_eq!(marker_column("\u{2591}"), 1, "shade");
        assert_eq!(marker_column("\u{958B}\u{767A}"), 4, "CJK run");
    }

    #[test]
    fn a_wide_glyph_forces_two_columns_and_blanks_its_trailing_cell() {
        let (buf, _) = painted("\u{2733}\u{FE0F}".as_bytes(), 6);
        assert_eq!(
            buf[(0, 0)].diff_option,
            CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap())
        );
        assert_eq!(
            buf[(1, 0)],
            ratatui::buffer::Cell::EMPTY,
            "ratatui requires the trailing cell of a wide glyph to be blank"
        );
    }

    #[test]
    fn a_hyperlink_run_is_opened_once_and_closed_once() {
        let (buf, _) = painted(b"\x1b]8;;https://example.com\x07abc\x1b]8;;\x07d", 10);
        let first = buf[(0, 0)].symbol();
        assert!(
            first.starts_with(&format!("{OSC8}https://example.com{ST}")),
            "the run opens on its first cell, got {first:?}"
        );
        assert!(first.ends_with('a'), "got {first:?}");
        assert_eq!(buf[(1, 0)].symbol(), "b", "inner cells stay plain");
        assert_eq!(
            buf[(2, 0)].symbol(),
            &format!("c{OSC8}{ST}"),
            "the close belongs on the last linked cell"
        );
        assert_eq!(
            buf[(3, 0)].symbol(),
            "d",
            "text after the link carries no escape"
        );
    }

    #[test]
    fn a_link_reaching_the_right_edge_is_still_closed() {
        // Left open, the host terminal would treat every following row as
        // part of the same link.
        let (buf, _) = painted(b"\x1b]8;;https://example.com\x07abc", 3);
        assert!(
            buf[(2, 0)].symbol().ends_with(&format!("{OSC8}{ST}")),
            "got {:?}",
            buf[(2, 0)].symbol()
        );
    }

    #[test]
    fn unlinked_text_carries_no_escape_at_all() {
        let (buf, _) = painted(b"plain", 8);
        for c in 0..5 {
            assert!(
                !buf[(c, 0)].symbol().contains('\u{1b}'),
                "col {c} should be bare"
            );
        }
    }
}
