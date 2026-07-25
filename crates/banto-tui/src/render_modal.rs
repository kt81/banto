//! Rendering [`Modal`] as a centered overlay — pure `(frame, modal, area)`
//! rendering shared by both the chōba list TUI (`banto::tui`) and the
//! emporium (`banto::embedded::emporium`); each imports [`render_modal`]
//! back rather than rendering modals independently, so both modes render
//! identically. [`windowed_view`]/[`modal_area`] (and [`crate::text`]'s
//! truncation helpers) are also reused by each mode's own non-modal
//! rendering (a search box, a splash screen), so they're `pub` here too
//! rather than only exposed via `render_modal`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use banto_core::app::{GroupJoinState, Modal, NewSessionPlacement, NewSessionState};

use crate::text::truncate_to_width;

/// Minimum margin (columns/rows) kept around a modal, even in a narrow pane.
const MODAL_MIN_MARGIN: u16 = 2;
/// Below this width, the pane counts as "narrow" (the user mostly runs
/// banto in a tall, narrow pane): the modal fills nearly the whole width
/// instead of leaving a percentage-based margin.
const MODAL_NARROW_THRESHOLD: u16 = 90;
/// In a pane at or above [`MODAL_NARROW_THRESHOLD`], the modal uses this
/// percentage of the width, before the [`MODAL_MAX_WIDTH`] cap.
const MODAL_WIDE_WIDTH_PERCENT: u32 = 60;
/// A modal's width never exceeds this even in a very wide pane, so it still
/// gets a comfortable margin instead of stretching edge to edge.
const MODAL_MAX_WIDTH: u16 = 80;
const MODAL_MAX_HEIGHT: u16 = 20;

/// Center a modal box within `area`. Width: below [`MODAL_NARROW_THRESHOLD`]
/// the modal fills nearly the whole pane (just [`MODAL_MIN_MARGIN`] on each
/// side); at or above it, the modal uses [`MODAL_WIDE_WIDTH_PERCENT`] of the
/// width, capped at [`MODAL_MAX_WIDTH`] so a very wide pane still gets a
/// generous margin instead of an edge-to-edge dialog. Height uses the same
/// minimal-margin/capped-max shape as the narrow-width case, since the
/// user's typical pane is already tall.
pub fn modal_area(area: Rect) -> Rect {
    let width = if area.width < MODAL_NARROW_THRESHOLD {
        area.width.saturating_sub(MODAL_MIN_MARGIN * 2)
    } else {
        let wide_width = area.width as u32 * MODAL_WIDE_WIDTH_PERCENT / 100;
        wide_width.min(MODAL_MAX_WIDTH as u32) as u16
    }
    .max(1);
    let height = area
        .height
        .saturating_sub(MODAL_MIN_MARGIN * 2)
        .clamp(1, MODAL_MAX_HEIGHT);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

/// The area [`render_modal`] should pass to [`Clear`] for a modal box that
/// itself occupies [`modal_area`]`(full_area)`: [`modal_area`]'s box widened
/// by one column on each side, clamped to `full_area`'s own bounds.
///
/// This is one column wider than the box on purpose. A background row's
/// full-width character (e.g. Japanese) can, by coincidence, have its glyph
/// cell sit one column to the *left* of the box while its blank
/// "continuation" cell (see `ratatui_core::buffer::Buffer::set_string`)
/// lands exactly on the box's own left border column. If that glyph cell is
/// left untouched by `Clear` (i.e. only [`modal_area`] is cleared) and
/// therefore unchanged from the previous frame, `ratatui`'s buffer-diffing
/// (`ratatui_core::buffer::diff`) treats it as an unmodified double-width
/// cell and — assuming the terminal's own rendering of it already covers
/// the next column — skips diffing that next column at all. Any different
/// content the border widget then writes into that "continuation" cell
/// (e.g. the box's own left border) is silently dropped rather than sent to
/// the backend, so the border never actually gets (re)drawn there
/// (dogfooding report; confirmed by inspecting the drawn `TestBackend`
/// buffer directly). Widening the cleared area by one column blanks that
/// glyph cell too, which makes it match the previous frame's already-blank
/// state and stops the diff from skipping the border's own cell.
pub fn modal_clear_area(full_area: Rect) -> Rect {
    let modal = modal_area(full_area);
    let x = modal.x.saturating_sub(1).max(full_area.x);
    let right = (modal.x + modal.width + 1).min(full_area.x + full_area.width);
    Rect::new(x, modal.y, right.saturating_sub(x), modal.height)
}

/// Inset `area` by one column on the left and right, leaving its vertical
/// extent untouched — modal content (input text, candidate lists, the
/// archive-confirm prompt) was rendered flush against the box's left/right
/// border with no breathing room (dogfooding report).
fn pad_horizontal(area: Rect) -> Rect {
    area.inner(Margin::new(1, 0))
}

/// Compute the slice of `s` to display in a `max_width`-column single-line
/// input box so the cursor (`cursor`, a char index into `s`) stays visible,
/// plus the cursor's on-screen column relative to that slice (which the
/// caller still needs to clamp to `max_width - 1`, same as any other
/// in-box column, since a cursor at the very end of a maxed-out field sits
/// one column past the last visible character). Used by every editable
/// text input (the search box, the cwd/group-name modal inputs), now that
/// the cursor can sit anywhere in the string rather than always at the end.
///
/// Never splits a full-width character (see [`truncate_to_width`]): a
/// character whose column span crosses the window's edge is left out
/// entirely rather than half-drawn. Reduces to showing the tail of the
/// string when the cursor is at or near the end (matching a normal
/// terminal input box scrolling as you type), and the head when the cursor
/// is at or near the start.
pub fn windowed_view(s: &str, cursor: usize, max_width: u16) -> (String, u16) {
    let max_width = max_width as usize;
    if max_width == 0 {
        return (String::new(), 0);
    }
    let cursor_byte = s
        .char_indices()
        .nth(cursor)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let prefix_width = s[..cursor_byte].width();
    let total_width = s.width();
    if total_width <= max_width {
        return (s.to_string(), prefix_width as u16);
    }

    // Choose the window's start column so the cursor stays inside it,
    // clamped so the window never scrolls further right than the string's
    // own end (i.e. it shows the tail exactly, rather than trailing blank
    // space, once the cursor is near the end).
    let max_start = total_width - max_width;
    let start_col = prefix_width
        .saturating_sub(max_width.saturating_sub(1))
        .min(max_start);

    let mut visible = String::new();
    let mut col = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if col >= start_col {
            if col + w > start_col + max_width {
                break;
            }
            visible.push(c);
        }
        col += w;
    }
    let cursor_col = prefix_width.saturating_sub(start_col) as u16;
    (visible, cursor_col)
}

/// Render whichever modal is open as a centered overlay on top of the rest
/// of the UI: [`Clear`] blanks only the modal's own box (widened by one
/// column each side — see [`modal_clear_area`]), leaving the background list
/// visible in the margin around it — that's a modal's virtue, not a bug, and
/// worth keeping. A background row with a long full-width title (the common
/// case for this modal, since the archive/group-join prompts echo that very
/// session's own title) never actually overflows *into* the box: the box's
/// own content is always truncated to fit (see [`truncate_to_width`]), so
/// there is nothing to blank behind it — the one-column widen is solely to
/// neutralize a background full-width character straddling the border
/// itself (see [`modal_clear_area`]).
pub fn render_modal(frame: &mut Frame, modal: &Modal, full_area: Rect) {
    let area = modal_area(full_area);
    frame.render_widget(Clear, modal_clear_area(full_area));
    match modal {
        Modal::NewSession(state) => render_new_session_modal(frame, state, area),
        Modal::ConfirmArchive { title, .. } => render_confirm_archive_modal(frame, title, area),
        Modal::GroupJoin(state) => render_group_join_modal(frame, state, area),
        Modal::ConfirmDisband { name, .. } => render_confirm_disband_modal(frame, name, area),
        Modal::ConfirmKill { title, .. } => render_confirm_kill_modal(frame, title, area),
    }
}

/// Render the `n` new-session dialog: a one-line cwd input (with a blinking
/// cursor, same convention as the search box), an inline validation error
/// when the last confirm attempt failed (see `App::modal_set_error`), and
/// a substring-filtered list of previously seen cwds to pick from instead of
/// typing a full path (Tab completes the highlighted one into the input).
fn render_new_session_modal(frame: &mut Frame, state: &NewSessionState, area: Rect) {
    let placement_label = match state.placement() {
        NewSessionPlacement::InPlace => "in-place",
        NewSessionPlacement::Split => "split",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" New Session ({placement_label}) \u{2014} cwd "))
        .title_bottom(" Enter launch  Tab complete  Esc cancel ");
    let inner = pad_horizontal(block.inner(area));
    frame.render_widget(block, area);

    let [input_area, error_area, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    let (visible_input, cursor_col) =
        windowed_view(state.input(), state.cursor(), input_area.width);
    frame.render_widget(Paragraph::new(visible_input.as_str()), input_area);
    if input_area.width > 0 {
        let cursor_x = (input_area.x + cursor_col).min(input_area.x + input_area.width - 1);
        frame.set_cursor_position(Position::new(cursor_x, input_area.y));
    }

    if let Some(error) = state.error() {
        frame.render_widget(
            Paragraph::new(truncate_to_width(error, error_area.width))
                .style(Style::default().fg(Color::Red)),
            error_area,
        );
    }

    let candidates = state.candidates();
    if candidates.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching directories \u{2014} Enter uses the typed path.")
                .style(Style::default().fg(Color::DarkGray)),
            list_area,
        );
        return;
    }

    let items: Vec<ListItem> = candidates
        .iter()
        .map(|candidate| ListItem::new(truncate_to_width(candidate, list_area.width)))
        .collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut list_state = ListState::default();
    list_state.select(state.selected());
    frame.render_stateful_widget(list, list_area, &mut list_state);
}

/// Render the `d` archive confirm dialog: a one-line yes/no prompt naming
/// the session. Archiving only soft-hides it (`Store::archive_session`) —
/// the real jsonl file is never touched, and unarchiving isn't exposed in
/// the UI yet, so the prompt says as much to set expectations.
fn render_confirm_archive_modal(frame: &mut Frame, title: &str, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Archive Session \u{2014} confirm ")
        .title_bottom(" Enter archive  Esc cancel ");
    let inner = pad_horizontal(block.inner(area));
    frame.render_widget(block, area);

    let prompt = truncate_to_width(&format!("Archive \"{title}\"?"), inner.width);
    let lines = vec![
        Line::from(prompt),
        Line::from(Span::styled(
            "Hides it from the list; the session file itself is untouched.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the emporium's `B`-on-a-Director disband confirm dialog: a
/// one-line yes/no prompt naming the brigade's Director. Its Workers'
/// `claude` processes are left running (they simply reappear in the list as
/// live sessions once the brigade's hiding is gone), so the prompt says as
/// much to set expectations.
fn render_confirm_disband_modal(frame: &mut Frame, name: &str, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Disband Brigade \u{2014} confirm ")
        .title_bottom(" Enter disband  Esc cancel ");
    let inner = pad_horizontal(block.inner(area));
    frame.render_widget(block, area);

    let prompt = truncate_to_width(
        &format!("Disband the brigade led by \"{name}\"?"),
        inner.width,
    );
    let lines = vec![
        Line::from(prompt),
        Line::from(Span::styled(
            "Its Workers keep running and simply reappear in the list.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the emporium's prefix-`x` kill confirm dialog: a one-line yes/no
/// prompt naming the session. Killing only ends the process — brigade
/// membership is untouched, so a killed Worker respawns fresh under the same
/// token the next time its brigade is staged (the same disposable-Worker
/// semantics `stage_brigade` already has for one that's simply gone).
fn render_confirm_kill_modal(frame: &mut Frame, title: &str, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Kill Session \u{2014} confirm ")
        .title_bottom(" Enter kill  Esc cancel ");
    let inner = pad_horizontal(block.inner(area));
    frame.render_widget(block, area);

    let prompt = truncate_to_width(&format!("Kill \"{title}\"?"), inner.width);
    let lines = vec![
        Line::from(prompt),
        Line::from(Span::styled(
            "Ends the process now. A Worker respawns fresh next time its \
             brigade is staged; a Director does not.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the `g` group-join dialog: a one-line new-group-name input (same
/// input/cursor convention as the search box and the new-session modal)
/// above a substring-filtered list of existing groups to pick from instead.
fn render_group_join_modal(frame: &mut Frame, state: &GroupJoinState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(" Join Group ")
        .title_bottom(" Enter join/create  Esc cancel ");
    let inner = pad_horizontal(block.inner(area));
    frame.render_widget(block, area);

    let [hint_area, input_area, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(truncate_to_width(
            "Type a new group name, or pick an existing one below:",
            hint_area.width,
        ))
        .style(Style::default().fg(Color::DarkGray)),
        hint_area,
    );

    let (visible_input, cursor_col) =
        windowed_view(state.input(), state.cursor(), input_area.width);
    frame.render_widget(Paragraph::new(visible_input.as_str()), input_area);
    if input_area.width > 0 {
        let cursor_x = (input_area.x + cursor_col).min(input_area.x + input_area.width - 1);
        frame.set_cursor_position(Position::new(cursor_x, input_area.y));
    }

    let candidates = state.candidates();
    if candidates.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching groups \u{2014} Enter creates a new one.")
                .style(Style::default().fg(Color::DarkGray)),
            list_area,
        );
        return;
    }

    let items: Vec<ListItem> = candidates
        .iter()
        .map(|candidate| ListItem::new(truncate_to_width(candidate, list_area.width)))
        .collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut list_state = ListState::default();
    list_state.select(state.selected());
    frame.render_stateful_widget(list, list_area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn modal_area_shrinks_margin_in_a_narrow_pane_scales_by_percentage_in_a_mid_one_and_caps_in_a_wide_one()
     {
        // Narrow: minimal margin, modal fills almost the whole width.
        let narrow = modal_area(Rect::new(0, 0, 30, 20));
        assert_eq!(narrow.width, 26); // 30 - 2*MODAL_MIN_MARGIN
        assert_eq!(narrow.x, 2);

        // Mid: at/above the narrow threshold but under the cap, the modal
        // uses MODAL_WIDE_WIDTH_PERCENT of the width.
        let mid = modal_area(Rect::new(0, 0, 100, 30));
        assert_eq!(mid.width, 60); // 100 * 60 / 100
        assert_eq!(mid.x, 20); // centered: (100 - 60) / 2

        // Wide: capped at MODAL_MAX_WIDTH, leaving a large margin.
        let wide = modal_area(Rect::new(0, 0, 200, 50));
        assert_eq!(wide.width, 80); // MODAL_MAX_WIDTH
        assert_eq!(wide.x, 60); // centered: (200 - 80) / 2
    }

    #[test]
    fn modal_clear_area_widens_the_modal_box_by_one_column_on_each_side() {
        let full = Rect::new(0, 0, 40, 15);
        let modal = modal_area(full);

        let clear = modal_clear_area(full);

        assert_eq!(clear.x, modal.x - 1);
        assert_eq!(clear.width, modal.width + 2);
        assert_eq!(clear.y, modal.y);
        assert_eq!(clear.height, modal.height);
    }

    #[test]
    fn modal_clear_area_clamps_at_the_frame_edges_in_a_maximally_narrow_pane() {
        // A pane so narrow that modal_area is flush against x=0 (its own
        // saturating-sub clamp already kicks in); the widened clear area
        // must not underflow past the frame's own left edge, nor extend
        // past its right edge.
        let full = Rect::new(0, 0, 1, 10);
        assert_eq!(modal_area(full).x, 0);

        let clear = modal_clear_area(full);

        assert!(clear.x >= full.x);
        assert!(clear.x + clear.width <= full.x + full.width);
    }

    #[test]
    fn a_long_title_in_the_archive_modal_is_truncated_with_an_ellipsis() {
        let long_title = "あ".repeat(60);
        let area = Rect::new(2, 2, 20, 6);
        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        terminal
            .draw(|frame| render_confirm_archive_modal(frame, &long_title, area))
            .unwrap();
        let buf = terminal.backend().buffer();

        let title_row = area.y + 1;
        let right_border_x = area.x + area.width - 1;
        assert_eq!(
            buf.cell((right_border_x, title_row)).unwrap().symbol(),
            "\u{2502}",
            "the block's own right border must survive"
        );
        assert_eq!(
            buf.cell((right_border_x - 1, title_row)).unwrap().symbol(),
            " ",
            "the 1-column right padding must stay blank"
        );
        let row_text: String = (area.x..=right_border_x)
            .map(|x| buf.cell((x, title_row)).unwrap().symbol().to_string())
            .collect();
        assert!(
            row_text.contains('\u{2026}'),
            "long content should end in a visible ellipsis, not a silent cutoff:\n{row_text}"
        );
    }

    #[test]
    fn render_modal_neutralizes_a_background_full_width_char_straddling_the_left_border() {
        let modal = Modal::ConfirmArchive {
            session_id: "a".to_string(),
            title: "Alpha".to_string(),
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                // Simulate a background full-width character whose glyph
                // cell sits exactly 1 column left of the modal's own left
                // border (x=2, see
                // `modal_area_shrinks_margin_in_a_narrow_pane_...`), with its
                // blank "continuation" cell (see
                // `ratatui_core::buffer::Buffer::set_string`) landing
                // exactly on the border column — the scenario that used to
                // leave a dangling half-glyph once the border overwrote
                // only the continuation cell.
                frame.render_widget(Span::raw("あ"), Rect::new(1, 5, 2, 1));
                render_modal(frame, &modal, area);
            })
            .unwrap();
        let buf = terminal.backend().buffer();

        // The border survived intact...
        assert_eq!(buf.cell((2, 5)).unwrap().symbol(), "\u{2502}");
        // ...and the dangling glyph was neutralized (blanked) instead of
        // being left half-erased.
        assert_eq!(buf.cell((1, 5)).unwrap().symbol(), " ");
    }

    #[test]
    fn render_modal_neutralizes_a_background_full_width_char_straddling_the_right_border() {
        let modal = Modal::ConfirmArchive {
            session_id: "a".to_string(),
            title: "Alpha".to_string(),
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                // Mirror of the left-border case: glyph at column 38, just
                // past the box's own right border at column 37 (see
                // `modal_area_shrinks_margin_in_a_narrow_pane_...`:
                // width 36 starting at x=2 ends at column 37).
                frame.render_widget(Span::raw("あ"), Rect::new(38, 5, 2, 1));
                render_modal(frame, &modal, area);
            })
            .unwrap();
        let buf = terminal.backend().buffer();

        // The right border survived intact...
        assert_eq!(buf.cell((37, 5)).unwrap().symbol(), "\u{2502}");
        // ...and the adjacent glyph was neutralized rather than left
        // dangling at the very edge of the frame.
        assert_eq!(buf.cell((38, 5)).unwrap().symbol(), " ");
    }

    #[test]
    fn windowed_view_shows_the_tail_when_the_cursor_is_at_the_end() {
        let (visible, cursor_col) = windowed_view("hello world", 11, 5);
        assert_eq!(visible, "world");
        // One past the last visible column; the caller clamps this into the
        // box, same as any other cursor position.
        assert_eq!(cursor_col, 5);
    }

    #[test]
    fn windowed_view_shows_the_head_when_the_cursor_is_at_the_start() {
        let (visible, cursor_col) = windowed_view("hello world", 0, 5);
        assert_eq!(visible, "hello");
        assert_eq!(cursor_col, 0);
    }

    #[test]
    fn windowed_view_keeps_the_cursor_visible_when_editing_mid_string() {
        // cursor at char-index 8 ("hello wo|rld"), a 6-column window.
        let (visible, cursor_col) = windowed_view("hello world", 8, 6);
        assert_eq!(visible, "lo wor");
        assert_eq!(cursor_col, 5);
    }

    #[test]
    fn windowed_view_never_splits_a_full_width_character() {
        let (visible, cursor_col) = windowed_view(&"あ".repeat(3), 3, 3);
        assert_eq!(visible, "あ");
        assert_eq!(cursor_col, 3);
    }

    #[test]
    fn windowed_view_cursor_column_accounts_for_a_full_width_character_before_it() {
        // "aあb": cursor after char-index 2 (past 'a' and 'あ'). Width-wise
        // that's column 3 (1 + 2), not char-count 2 — this is what a
        // width-aware cursor column must get right that a naive
        // `chars().count()` one would not.
        let (visible, cursor_col) = windowed_view("aあb", 2, 10);
        assert_eq!(visible, "aあb");
        assert_eq!(cursor_col, 3);
    }
}
