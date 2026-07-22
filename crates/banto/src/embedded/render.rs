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
    use ratatui::style::Color;

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
}
