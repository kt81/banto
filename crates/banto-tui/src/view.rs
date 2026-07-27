//! Shared session-list / summary rendering, used by both the chōba list TUI
//! (`banto::tui`) and the emporium sidebar (`banto::embedded::emporium`).
//!
//! These are pure `(frame, app, area, now)` widgets so either mode can place
//! them in its own layout; all list/selection/scroll state lives in
//! `banto_core::app::App`, which both modes drive. Extracted from the chōba
//! TUI so the two modes render sessions identically rather than drifting.
//!
//! # Emoji markers
//!
//! Exactly six, fixed: 🤝 director (the partnership metaphor — Director and
//! Worker as a cell — not royalty; a prior busts-in-silhouette choice was
//! a dark, low-contrast glyph that all but vanished on a dark terminal
//! background), 🧬 superseded (an auto-compaction ancestor with a known
//! continuation), 📌 pinned, 🤖 agent, 📂 named-group header, 📁 Ungrouped
//! header — all single-codepoint, East-Asian-Width=Wide,
//! default-emoji-presentation characters (2 display columns, no VS16
//! sequence), so they hold their column budget in a grid exactly like a
//! full-width CJK character would. The activity dot stays the plain "●"
//! (U+25CF) + theme color it always was — deliberately NOT a colored-circle
//! emoji, which would lose the theme colors and this codebase's
//! production-proven narrow (1-column) rendering.
//!
//! # Colour
//!
//! Truecolor is assumed; the 16- and 256-colour palettes are not a
//! constraint on what may be chosen here (operator's call — the terminals
//! this is used from are all rich, and the agents it hosts paint freely
//! themselves). Named colours are still the right default for *status*,
//! where following the reader's theme is the point — the activity dot
//! keeps them. Identity is the exception: an agent's accent names an
//! absolute colour, because the point of it is that a product looks like
//! itself rather than like the reader's palette.
//!
//! # Row layout
//!
//! One algorithm, driven by the render area's width and whether grouped
//! view is actually in effect (`App::grouped_view_in_effect`), for both
//! modes: `[dot 2][pin 0-or-3][role 3][title][gap 2][cwd?][gap][label][gap>=2][age]`.
//! The pin slot exists (3 columns) only in flat view — in grouped view
//! every pinned row's section is "Pinned" (`App::section_name` gives it
//! top priority), so a pin marker could never render there anyway; the
//! column is dropped entirely rather than reserved-but-always-blank. The
//! role slot is unconditional in both modes. A present marker slot is the
//! emoji plus one trailing separator space — a glued-together `📌🤝title`
//! reads as one sticker, not two markers. See [`row_line`] for the exact
//! budget arithmetic (title/cwd mutually exclusive truncation, age always
//! flush at the right edge, saturating throughout with an explicit
//! narrow-width degradation that drops the age column entirely rather than
//! ever let it collide into the prefix).
//!
//! A row is one line (`App::lines_per_row` 1, the chōba — the label sits on
//! that same line, per the formula above) or two (more than 1, the emporium
//! sidebar): line one is the formula above minus the label, line two is cwd
//! plus the label, indented to align under the title rather than starting
//! at column zero — see [`cwd_agent_line`].

use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use unicode_width::UnicodeWidthStr;

use banto_core::app::{App, ListLine, VisibleRow};
use banto_core::model::{self, Activity, AgeBucket, AgentKind, SessionRow};

use crate::text::{truncate_to_width, truncate_to_width_leading};

const PIN_EMOJI: &str = "\u{1F4CC}"; // 📌
const DIRECTOR_EMOJI: &str = "\u{1F91D}"; // 🤝
const SUPERSEDED_EMOJI: &str = "\u{1F9EC}"; // 🧬
const AGENT_EMOJI: &str = "\u{1F916}"; // 🤖
const GROUP_EMOJI: &str = "\u{1F4C2}"; // 📂
const UNGROUPED_EMOJI: &str = "\u{1F4C1}"; // 📁

/// dot(2) + pin(0 or 3) + role(3), the row's fixed left-hand slots — see the
/// module doc's "Row layout" section for why the pin slot is conditional.
/// A present slot is a 2-column emoji plus a 1-column trailing separator
/// space, not 2, so a marker never sits glued against the title or against
/// an adjacent marker.
fn fixed_prefix_width(show_pin_slot: bool) -> usize {
    let pin_width = if show_pin_slot { 3 } else { 0 };
    2 + pin_width + 3
}
const MIN_GAP_BEFORE_AGE: usize = 2;
const TITLE_CWD_GAP: usize = 2;
/// Gap between the title/cwd content and the agent label, when both share
/// one physical line (the chōba — see [`App::lines_per_row`]; the emporium
/// sidebar puts the label on its own second line instead, via
/// [`cwd_agent_line`]).
const LABEL_GAP: usize = 2;
/// cwd is only shown if at least this many columns remain for it once the
/// title (at its natural, untruncated width) and the title-cwd gap are
/// accounted for.
const MIN_CWD_WIDTH: usize = 8;
/// cwd is never shown below this render width, regardless of how much room
/// the title leaves — at sidebar widths (e.g. the emporium's 34-col list),
/// a short title's leftover space isn't a genuine invitation to show cwd,
/// it just produces a cramped, barely-readable fragment. Wide areas keep
/// the existing [`MIN_CWD_WIDTH`]-based rule unchanged.
const MIN_WIDTH_FOR_CWD: usize = 60;

/// Render the session list (or a placeholder when nothing matches) into
/// `area`. `now` drives the right-aligned compact age column — read once by
/// the caller at the draw call's boundary, same as [`render_summary`].
pub fn render_list(frame: &mut Frame, app: &App, area: Rect, now: SystemTime) {
    if app.filtered_len() == 0 {
        // A restricted `agents` setting narrowing discovery down to zero
        // rows looks identical, at this point, to a machine that genuinely
        // has none — distinguished only by `App::restricted_agents_label`,
        // set from `Config.agents` at startup, never by anything filtered
        // here. Checked only in the `total_len() == 0` arm: a search query
        // narrowing an otherwise-populated list to nothing is a completely
        // different, already-self-explanatory situation ("No matching
        // sessions."), not this one.
        let message = if app.total_len() == 0 {
            match app.restricted_agents_label() {
                Some(label) => format!("No sessions found (agents = {label})."),
                None => "No sessions found.".to_string(),
            }
        } else {
            "No matching sessions.".to_string()
        };
        let placeholder = Paragraph::new(message).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(placeholder, area);
        return;
    }

    // See the module doc's "Row layout" section for why the pin slot is
    // conditional on this.
    let show_pin_slot = !app.grouped_view_in_effect();
    let lines_per_row = app.lines_per_row();
    let items: Vec<ListItem> = app
        .visible()
        .into_iter()
        .map(|line| list_item(line, area.width, now, show_pin_slot, lines_per_row))
        .collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    state.select(app.selected_in_viewport());
    frame.render_stateful_widget(list, area, &mut state);
}

/// Build one list line: a bold, icon-prefixed section-header line (grouped
/// view only), or a row. Each is its own `ListItem`/physical-line-group
/// rather than a header bundled into its row, matching the index space
/// `App::click`/`App::scroll`/`App::ensure_visible` all use — see
/// [`ListLine`] for why that matters for mouse clicks. A row is one line
/// when `lines_per_row` is 1 (the chōba, with the agent label sharing that
/// line — see [`row_line`]), or two when it's more (the emporium sidebar:
/// title/age, then cwd/agent — see [`cwd_agent_line`]); a header is always
/// one line regardless, matching `App::lines_per_row`'s own accounting.
fn list_item(
    line: ListLine<'_>,
    area_width: u16,
    now: SystemTime,
    show_pin_slot: bool,
    lines_per_row: usize,
) -> ListItem<'static> {
    match line {
        ListLine::Header { name, count } => header_line(&name, count),
        ListLine::Row(visible) => {
            if lines_per_row > 1 {
                let top = row_line(&visible, area_width, now, show_pin_slot, None);
                let bottom = cwd_agent_line(visible.row, area_width, show_pin_slot);
                ListItem::new(vec![top, bottom])
            } else {
                let label = agent_label(visible.row);
                ListItem::new(row_line(
                    &visible,
                    area_width,
                    now,
                    show_pin_slot,
                    Some(&label),
                ))
            }
        }
    }
}

/// One list-row marker slot: `emoji` plus a trailing separator space (3
/// display columns total, matching [`fixed_prefix_width`]'s pin/role
/// budget) when occupied, or three blank columns when `None` — so an empty
/// slot still holds its column budget exactly like an occupied one.
fn marker_slot(emoji: Option<&str>) -> Span<'static> {
    match emoji {
        Some(emoji) => Span::raw(format!("{emoji} ")),
        None => Span::raw("   "),
    }
}

/// A section header: "📌 Pinned (2)", "📂 <name> (n)", or "📁 Ungrouped (n)"
/// — icon chosen by the header's own name (the only three shapes
/// `App::section_name` ever produces), `count` is that section's row count
/// under the current filter.
fn header_line(name: &str, count: usize) -> ListItem<'static> {
    let icon = match name {
        "Pinned" => PIN_EMOJI,
        "Ungrouped" => UNGROUPED_EMOJI,
        _ => GROUP_EMOJI,
    };
    ListItem::new(Line::from(Span::styled(
        format!("{icon} {name} ({count})"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )))
}

/// Build one row's first (and, at `lines_per_row` 1, only) physical line:
/// `[dot 2][pin 0-or-3][role 3][title][gap 2][cwd?][gap][label?][gap>=2][age]`
/// — see the module doc's "Row layout" section for why the pin slot is
/// conditional on `show_pin_slot`. `label` is `None` when the emporium
/// sidebar's second line ([`cwd_agent_line`]) carries it instead; `Some` for
/// the chōba, where the row stays one line.
///
/// `title` and `cwd` are mutually exclusive truncation targets: cwd is only
/// shown when the title's *natural* (untruncated) width already leaves at
/// least [`MIN_CWD_WIDTH`] columns free for it — so whenever cwd is shown,
/// the title itself is never truncated; whenever the title doesn't fit that
/// budget (or there's no cwd to show at all), cwd is dropped and the title
/// gets truncated to whatever's left instead. `age` is always flush at the
/// row's literal right edge; the gap before it is a consequence of that
/// placement, not separately computed/padded.
///
/// All arithmetic is saturating. If the area is too narrow to fit the fixed
/// prefix, the minimum gap, `label` (if present), and age all at once, both
/// `label` and age are dropped together — not one before the other, so this
/// stays the same single narrow/normal split the row always had rather than
/// a new three-tier cascade — and the title gets whatever width remains
/// after the prefix, degrading further to an empty title, then (ratatui's
/// own area-boundary clipping) to just the prefix, in a pathologically
/// narrow area. Never panics, never underflows.
fn row_line(
    visible: &VisibleRow<'_>,
    area_width: u16,
    now: SystemTime,
    show_pin_slot: bool,
    label: Option<&str>,
) -> Line<'static> {
    let dot = Span::styled(
        "\u{25cf} ",
        Style::default().fg(activity_color(visible.row.activity)),
    );
    let role = marker_slot(if visible.director {
        Some(DIRECTOR_EMOJI)
    } else if visible.superseded {
        Some(SUPERSEDED_EMOJI)
    } else if visible.row.is_agent {
        Some(AGENT_EMOJI)
    } else {
        None
    });
    let mut prefix = vec![dot];
    if show_pin_slot {
        prefix.push(marker_slot(visible.pinned.then_some(PIN_EMOJI)));
    }
    prefix.push(role);
    let prefix_width = fixed_prefix_width(show_pin_slot);

    let area_width = area_width as usize;
    let age_str = model::humanize_age_compact(visible.row.mtime, now);
    let age_width = age_str.width();
    let label_reserved = label.map_or(0, |label| LABEL_GAP + label.width());

    let title_full = visible.row.display_title().to_string();
    let title_width = title_full.width();
    let cwd_full = visible.row.cwd_display();

    let max_left_content =
        area_width.saturating_sub(prefix_width + label_reserved + MIN_GAP_BEFORE_AGE + age_width);

    if max_left_content == 0 {
        // Narrow-width degradation: no room for prefix + label (if any) +
        // min gap + age all at once — drop label and age together rather
        // than let anything collide into the prefix, and give the title
        // whatever's left after the prefix.
        let title_budget = area_width.saturating_sub(prefix_width);
        let title = truncate_to_width(&title_full, title_budget as u16);
        prefix.push(Span::raw(title));
        return Line::from(prefix);
    }

    let show_cwd = area_width >= MIN_WIDTH_FOR_CWD
        && !cwd_full.is_empty()
        && title_width + TITLE_CWD_GAP + MIN_CWD_WIDTH <= max_left_content;

    let mut spans = prefix;
    let mut used_width = prefix_width;
    if show_cwd {
        let cwd_budget = max_left_content - title_width - TITLE_CWD_GAP;
        used_width += title_width;
        spans.push(Span::raw(title_full));
        spans.push(Span::raw("  "));
        used_width += TITLE_CWD_GAP;
        let cwd_truncated = truncate_to_width_leading(&cwd_full, cwd_budget as u16);
        used_width += cwd_truncated.width();
        spans.push(Span::styled(
            cwd_truncated,
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        let title = truncate_to_width(&title_full, max_left_content as u16);
        used_width += title.width();
        spans.push(Span::raw(title));
    }

    if let Some(label) = label {
        spans.push(Span::raw(" ".repeat(LABEL_GAP)));
        used_width += LABEL_GAP;
        spans.push(Span::styled(
            label.to_string(),
            Style::default().fg(agent_label_color(visible.row.agent)),
        ));
        used_width += label.width();
    }

    // Age, right-flush at the row's literal right edge. `used_width` is
    // bounded (by construction of `max_left_content` and the truncation
    // helpers' own <= max_width guarantee) so this gap is always >=
    // MIN_GAP_BEFORE_AGE — see the function doc.
    let gap = area_width.saturating_sub(used_width + age_width);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(age_str, Style::default().fg(Color::DarkGray)));

    Line::from(spans)
}

/// Bracketed agent product label — "no single emoji names an agent
/// product" (the operator's own call), so this is the one row-list marker
/// that stays text instead of joining the module doc's "Emoji markers"
/// section.
fn agent_label(row: &SessionRow) -> String {
    format!("[{}]", row.agent.label())
}

/// Each product's own accent, absolute rather than themed — see the module
/// doc's "Colour" section for why identity is the one thing here that does
/// not follow the reader's palette. Claude's orange and Codex's teal are
/// each that product's own; they also happen to sit on opposite sides of
/// the wheel, which is what keeps them apart at a glance in a mixed list.
///
/// Both stay off the activity dot's hues so the two coloured things in one
/// row don't read as related.
fn agent_label_color(agent: AgentKind) -> Color {
    match agent {
        AgentKind::ClaudeCode => Color::Rgb(217, 119, 87),
        AgentKind::Codex => Color::Rgb(16, 163, 127),
    }
}

/// The emporium sidebar's second physical line per row (only when
/// `App::lines_per_row` is more than 1, see [`list_item`]): indented by
/// [`fixed_prefix_width`] so its content aligns under line one's *title*
/// rather than starting at column zero — without this a stray path would
/// appear to hang between every row instead of reading as that row's own
/// second line (dogfooding report). cwd is leading-truncated on the left;
/// the agent label sits right-aligned at the row's literal right edge — the
/// same right-flush-trailing-element and saturating-arithmetic conventions
/// [`row_line`]'s own age column uses, applied here since age itself has no
/// place on this line. Below room for the label plus its minimum gap, the
/// gap guarantee is the first thing dropped (matching [`row_line`]'s own
/// "drop the gap, not the content" order); the label itself is truncated,
/// never panicking, only once even that alone doesn't fit.
fn cwd_agent_line(row: &SessionRow, area_width: u16, show_pin_slot: bool) -> Line<'static> {
    let indent = fixed_prefix_width(show_pin_slot);
    let content_width = (area_width as usize).saturating_sub(indent);
    let indent_span = Span::raw(" ".repeat(indent));

    let label = agent_label(row);
    let label_width = label.width();
    let label_style = Style::default().fg(agent_label_color(row.agent));
    let cwd_full = row.cwd_display();

    let max_cwd = content_width.saturating_sub(MIN_GAP_BEFORE_AGE + label_width);
    if max_cwd == 0 {
        let label = truncate_to_width(&label, content_width as u16);
        return Line::from(vec![indent_span, Span::styled(label, label_style)]);
    }

    let cwd_truncated = truncate_to_width_leading(&cwd_full, max_cwd as u16);
    let gap = content_width.saturating_sub(cwd_truncated.width() + label_width);
    Line::from(vec![
        indent_span,
        Span::styled(cwd_truncated, Style::default().fg(Color::DarkGray)),
        Span::raw(" ".repeat(gap)),
        Span::styled(label, label_style),
    ])
}

/// Render the always-visible summary panel: the selected session's activity
/// dot + marker(s) + title, preview excerpt, cwd, and a meta line (relative
/// age, size, short id, pinned/agent markers). A top border is the only
/// visual separation. A zero-height `area` (a too-short terminal, per the
/// caller's layout) makes this a no-op. `now` is the caller's own clock read
/// (view functions are pure `(frame, app, area, now)` — no query during
/// drawing).
pub fn render_summary(frame: &mut Frame, app: &App, area: Rect, now: SystemTime) {
    if area.height == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Details ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(row) = app.selected_row() else {
        frame.render_widget(
            Paragraph::new("No session selected.").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };

    let pinned = app.is_selected_pinned();
    let director = app.is_selected_director();
    let superseded = app.is_selected_superseded();

    // Marker slots after the dot — same 📌/🤝/🧬/🤖 priority as the list row
    // (director beats superseded beats agent), but as free-flowing spans
    // with no blank-slot padding: this is prose, not a column-aligned grid.
    let mut title_spans = vec![Span::styled(
        "\u{25cf} ",
        Style::default().fg(activity_color(row.activity)),
    )];
    if pinned {
        title_spans.push(Span::raw(format!("{PIN_EMOJI} ")));
    }
    if director {
        title_spans.push(Span::raw(format!("{DIRECTOR_EMOJI} ")));
    } else if superseded {
        title_spans.push(Span::raw(format!("{SUPERSEDED_EMOJI} ")));
    } else if row.is_agent {
        title_spans.push(Span::raw(format!("{AGENT_EMOJI} ")));
    }
    title_spans.push(Span::styled(
        row.display_title().to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let title_line = Line::from(title_spans);

    let preview_line = Line::from(Span::styled(
        row.preview.as_deref().unwrap_or_default(),
        Style::default().fg(Color::DarkGray),
    ));
    let cwd_line = Line::from(Span::styled(
        row.cwd_display(),
        Style::default().fg(Color::DarkGray),
    ));
    let meta_line = Line::from(Span::styled(
        summary_meta(row, pinned, director, superseded, now),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(
        Paragraph::new(vec![title_line, preview_line, cwd_line, meta_line]),
        inner,
    );
}

/// Build the summary panel's meta line: relative age, size, short id, and any
/// markers (pinned/director/superseded/agent) that apply. Textual, not
/// iconic, by design (the title line above carries the icons).
fn summary_meta(
    row: &SessionRow,
    pinned: bool,
    director: bool,
    superseded: bool,
    now: SystemTime,
) -> String {
    let mut parts = vec![
        model::humanize_age(row.mtime, now),
        model::humanize_size(row.size),
        model::short_id(&row.id),
    ];
    if pinned {
        parts.push("pinned".to_string());
    }
    if director {
        parts.push("director".to_string());
    }
    if superseded {
        parts.push("superseded".to_string());
    }
    if row.is_agent {
        parts.push("agent".to_string());
    }
    parts.join("  \u{b7}  ")
}

fn activity_color(activity: Activity) -> Color {
    match activity {
        Activity::Busy => Color::Green,
        Activity::Alive => Color::Cyan,
        Activity::Idle(AgeBucket::Today) => Color::Yellow,
        Activity::Idle(AgeBucket::ThisWeek) => Color::Gray,
        Activity::Idle(AgeBucket::Older) => Color::DarkGray,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;
    use banto_core::model::AgentKind;

    fn row(id: &str, title: &str, cwd: &str, mtime: SystemTime) -> SessionRow {
        SessionRow {
            id: id.to_string(),
            agent: AgentKind::ClaudeCode,
            title: (!title.is_empty()).then(|| title.to_string()),
            cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
            activity: Activity::Idle(AgeBucket::Older),
            is_agent: false,
            preview: None,
            mtime,
            size: 0,
        }
    }

    fn agent_row(id: &str, title: &str, cwd: &str, mtime: SystemTime) -> SessionRow {
        SessionRow {
            is_agent: true,
            ..row(id, title, cwd, mtime)
        }
    }

    fn codex_row(id: &str, title: &str, cwd: &str, mtime: SystemTime) -> SessionRow {
        SessionRow {
            agent: AgentKind::Codex,
            ..row(id, title, cwd, mtime)
        }
    }

    fn buffer_text(buf: &Buffer) -> String {
        let area = buf.area;
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
            text.push('\n');
        }
        text
    }

    fn draw_list(app: &App, width: u16, height: u16, now: SystemTime) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_list(frame, app, frame.area(), now))
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    /// The column (buffer x-coordinate) where `line_substr` starts, on the
    /// rendered line that contains it. `buffer_text` pushes exactly one
    /// `cell.symbol()` per x-coordinate for every column in the row (see
    /// its own loop above), so a `char`-indexed (not byte-indexed) position
    /// into the reconstructed line corresponds 1:1 to the real terminal
    /// column — unlike a byte offset, which would drift once a preceding
    /// multi-byte marker glyph is involved.
    fn title_start_col(
        app: &App,
        width: u16,
        height: u16,
        now: SystemTime,
        line_substr: &str,
    ) -> usize {
        let text = draw_list(app, width, height, now);
        let line = text
            .lines()
            .find(|l| l.contains(line_substr))
            .unwrap_or_else(|| panic!("no row contains {line_substr:?}:\n{text}"));
        let first_char = line_substr.chars().next().unwrap();
        line.chars()
            .position(|c| c == first_char)
            .unwrap_or_else(|| panic!("{first_char:?} not found in line {line:?}"))
    }

    // --- empty-list placeholder --------------------------------------------

    #[test]
    fn empty_list_with_no_agents_restriction_shows_the_plain_message() {
        let now = SystemTime::UNIX_EPOCH;
        let app = App::new(vec![]);
        let text = draw_list(&app, 60, 10, now);
        assert!(text.contains("No sessions found."));
        assert!(!text.contains("agents ="));
    }

    #[test]
    fn empty_list_under_a_restricted_agents_setting_names_it() {
        let now = SystemTime::UNIX_EPOCH;
        let app = App::new(vec![]).with_enabled_agents([AgentKind::Codex].into_iter().collect());
        let text = draw_list(&app, 60, 10, now);
        assert!(
            text.contains("No sessions found (agents = Codex)."),
            "{text}"
        );
    }

    #[test]
    fn a_search_query_that_matches_nothing_ignores_the_agents_restriction() {
        // Narrowing an otherwise-populated list to nothing via search is a
        // different, already self-explanatory situation — the `agents`
        // hint belongs only to the "nothing was ever discovered" case.
        let now = SystemTime::UNIX_EPOCH;
        let mut app = App::new(vec![row("h1", "Claude session", "", now)])
            .with_enabled_agents([AgentKind::Codex].into_iter().collect());
        app.enter_search();
        for c in "no-such-title".chars() {
            app.push_char(c);
        }
        let text = draw_list(&app, 60, 10, now);
        assert!(text.contains("No matching sessions."));
        assert!(!text.contains("agents ="));
    }

    // --- marker slots ------------------------------------------------------

    #[test]
    fn marker_slots_show_the_right_emoji_per_row() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![
            row("plain", "Plain", "", now),
            row("pinned", "Pinned Row", "", now),
            row("director", "Director Row", "", now),
            agent_row("agent", "Agent Row", "", now),
        ])
        .with_pinned(["pinned".to_string()].into_iter().collect())
        .with_directors(["director".to_string()].into_iter().collect());
        app.set_viewport_height(10);
        app.toggle_grouped_view(); // flat: markers unrelated to sections
        app.toggle_agent_filter(); // agents are hidden by default

        let text = draw_list(&app, 60, 10, now);
        let line_for = |title: &str| {
            text.lines()
                .find(|l| l.contains(title))
                .unwrap_or_else(|| panic!("missing row for {title}:\n{text}"))
        };

        let plain = line_for("Plain");
        assert!(!plain.contains(PIN_EMOJI));
        assert!(!plain.contains(DIRECTOR_EMOJI));
        assert!(!plain.contains(AGENT_EMOJI));
        assert!(line_for("Pinned Row").contains(PIN_EMOJI));
        assert!(line_for("Director Row").contains(DIRECTOR_EMOJI));
        assert!(line_for("Agent Row").contains(AGENT_EMOJI));
    }

    #[test]
    fn pin_marker_absent_in_grouped_view_but_shown_in_flat_view() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![
            row("pinned", "Pinned Row", "", now),
            row("other", "Other Row", "", now),
        ])
        .with_pinned(["pinned".to_string()].into_iter().collect());
        app.set_viewport_height(10);

        let text = draw_list(&app, 60, 10, now);
        let line = text.lines().find(|l| l.contains("Pinned Row")).unwrap();
        assert!(
            !line.contains(PIN_EMOJI),
            "pin marker should not appear in grouped view:\n{text}"
        );

        app.toggle_grouped_view();
        let text = draw_list(&app, 60, 10, now);
        let line = text.lines().find(|l| l.contains("Pinned Row")).unwrap();
        assert!(
            line.contains(PIN_EMOJI),
            "pin marker should show on the row in flat view:\n{text}"
        );
    }

    #[test]
    fn grouped_view_drops_the_pin_column_so_titles_start_three_columns_earlier() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![
            row("pinned", "Marked", "", now),
            row("other", "Zzz", "", now),
        ])
        .with_pinned(["pinned".to_string()].into_iter().collect());
        app.set_viewport_height(10);

        let grouped_col = title_start_col(&app, 60, 10, now, "Marked");

        app.toggle_grouped_view(); // flat: the pin slot exists again
        let flat_col = title_start_col(&app, 60, 10, now, "Marked");

        assert_eq!(
            flat_col - grouped_col,
            3,
            "flat view's title should start exactly 3 columns later than \
             grouped view's (the dropped pin slot)"
        );
    }

    #[test]
    fn searching_flattens_the_view_so_the_pin_slot_stays_present() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![
            row("pinned", "Marked", "", now),
            row("other", "Zzz", "", now),
        ])
        .with_pinned(["pinned".to_string()].into_iter().collect());
        app.set_viewport_height(10);

        // Grouped-view (no pin slot) baseline column.
        let grouped_col = title_start_col(&app, 60, 10, now, "Marked");

        // An active search always flattens the view, even with the
        // grouped-view toggle still on.
        app.enter_search();
        for c in "Mark".chars() {
            app.push_char(c);
        }
        let searching_col = title_start_col(&app, 60, 10, now, "Marked");
        assert_eq!(
            searching_col - grouped_col,
            3,
            "searching should flatten the view and restore the pin slot"
        );

        let text = draw_list(&app, 60, 10, now);
        let line = text.lines().find(|l| l.contains("Marked")).unwrap();
        assert!(
            line.contains(PIN_EMOJI),
            "pin marker should show while searching:\n{text}"
        );
    }

    #[test]
    fn pinned_and_ungrouped_section_rows_align_at_the_same_title_column() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![
            row("pinned", "Marked", "", now),
            row("other", "Zzz", "", now),
        ])
        .with_pinned(["pinned".to_string()].into_iter().collect());
        app.set_viewport_height(10);

        // The dropped pin column applies uniformly across sections, not
        // just to pinned rows.
        let pinned_col = title_start_col(&app, 60, 10, now, "Marked");
        let ungrouped_col = title_start_col(&app, 60, 10, now, "Zzz");
        assert_eq!(
            pinned_col,
            ungrouped_col,
            "Pinned-section and Ungrouped-section rows should align:\n{}",
            draw_list(&app, 60, 10, now)
        );
    }

    #[test]
    fn director_marker_takes_priority_over_agent_marker() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![agent_row("both", "Both Row", "", now)])
            .with_directors(["both".to_string()].into_iter().collect());
        app.set_viewport_height(10);
        app.toggle_agent_filter(); // agents are hidden by default

        let text = draw_list(&app, 60, 10, now);
        let line = text.lines().find(|l| l.contains("Both Row")).unwrap();
        assert!(line.contains(DIRECTOR_EMOJI));
        assert!(!line.contains(AGENT_EMOJI));
    }

    #[test]
    fn superseded_marker_shown_for_a_superseded_row() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![row("s", "Superseded Row", "", now)])
            .with_superseded(["s".to_string()].into_iter().collect());
        app.set_viewport_height(10);
        app.toggle_agent_filter(); // superseded rows are hidden by default too

        let text = draw_list(&app, 60, 10, now);
        let line = text.lines().find(|l| l.contains("Superseded Row")).unwrap();
        assert!(line.contains(SUPERSEDED_EMOJI));
    }

    #[test]
    fn director_marker_takes_priority_over_superseded_marker() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![row("both", "Both Row", "", now)])
            .with_directors(["both".to_string()].into_iter().collect())
            .with_superseded(["both".to_string()].into_iter().collect());
        app.set_viewport_height(10);
        app.toggle_agent_filter(); // reveal the superseded row

        let text = draw_list(&app, 60, 10, now);
        let line = text.lines().find(|l| l.contains("Both Row")).unwrap();
        assert!(line.contains(DIRECTOR_EMOJI));
        assert!(!line.contains(SUPERSEDED_EMOJI));
    }

    #[test]
    fn superseded_marker_takes_priority_over_agent_marker() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![agent_row("both", "Both Row", "", now)])
            .with_superseded(["both".to_string()].into_iter().collect());
        app.set_viewport_height(10);
        app.toggle_agent_filter(); // agents/superseded are hidden by default

        let text = draw_list(&app, 60, 10, now);
        let line = text.lines().find(|l| l.contains("Both Row")).unwrap();
        assert!(line.contains(SUPERSEDED_EMOJI));
        assert!(!line.contains(AGENT_EMOJI));
    }

    #[test]
    fn summary_panel_title_line_shows_markers_with_no_blank_padding() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![row("p", "Pinned Session", "", now)])
            .with_pinned(["p".to_string()].into_iter().collect());
        app.set_viewport_height(10);

        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        terminal
            .draw(|frame| render_summary(frame, &app, frame.area(), now))
            .unwrap();
        let buf = terminal.backend().buffer();

        // Title line is row 1 (row 0 is the top border). Find the exact
        // on-screen column "Pinned Session" starts at, via the buffer
        // directly rather than reconstructed text — reconstructing text
        // cell-by-cell would insert an extra blank after the emoji (see
        // `header_rendering_shows_icon_and_count`'s comment) and make exact
        // adjacency unverifiable that way.
        let title_row = 1;
        let start_col = (0..buf.area.width)
            .find(|&x| buf.cell((x, title_row)).unwrap().symbol() == "P")
            .expect("title not found on the expected row");

        // dot "\u{25cf} " (2 cells) + pin emoji (glyph + ratatui's own
        // continuation cell = 2) + one literal separator space = 5. If a
        // blank director/agent slot were reserved (matching the list row's
        // grid), this would be 7 instead.
        assert_eq!(
            start_col, 5,
            "expected no reserved blank marker slot before the summary title"
        );
        assert!(
            (0..buf.area.width).any(|x| buf.cell((x, title_row)).unwrap().symbol() == PIN_EMOJI),
            "pin marker missing from the title row"
        );
    }

    #[test]
    fn summary_panel_shows_the_superseded_marker_and_meta_text() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![row("s", "Superseded Session", "", now)])
            .with_superseded(["s".to_string()].into_iter().collect());
        app.set_viewport_height(10);
        app.toggle_agent_filter(); // reveal the superseded row so it can be selected

        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        terminal
            .draw(|frame| render_summary(frame, &app, frame.area(), now))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(
            text.contains(SUPERSEDED_EMOJI),
            "expected the DNA marker in the summary panel:\n{text}"
        );
        assert!(
            text.contains("superseded"),
            "expected \"superseded\" in the meta line:\n{text}"
        );
    }

    // --- headers -------------------------------------------------------

    #[test]
    fn header_rendering_shows_icon_and_count() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![
            row("a", "Alpha", "", now),
            row("b", "Beta", "", now),
            row("c", "Gamma", "", now),
        ])
        .with_pinned(["a".to_string()].into_iter().collect())
        .with_groups(
            vec![(1, "work".to_string())],
            [("b".to_string(), 1)].into_iter().collect(),
        );
        app.set_viewport_height(10);

        // Icon and text are checked on the same line, not as one adjacent
        // substring: a wide emoji occupies two buffer cells (its glyph, plus
        // ratatui's own blank "continuation" cell — see
        // `buffer/buffer.rs::set_stringn`), so reconstructing text
        // cell-by-cell inserts an extra blank after every emoji that isn't
        // present in the source `format!` string itself.
        let text = draw_list(&app, 60, 10, now);
        let pinned_header = text
            .lines()
            .find(|l| l.contains("Pinned (1)"))
            .unwrap_or_else(|| panic!("missing Pinned header:\n{text}"));
        assert!(pinned_header.contains(PIN_EMOJI), "{pinned_header}");
        let group_header = text
            .lines()
            .find(|l| l.contains("work (1)"))
            .unwrap_or_else(|| panic!("missing named-group header:\n{text}"));
        assert!(group_header.contains(GROUP_EMOJI), "{group_header}");
        let ungrouped_header = text
            .lines()
            .find(|l| l.contains("Ungrouped (1)"))
            .unwrap_or_else(|| panic!("missing Ungrouped header:\n{text}"));
        assert!(
            ungrouped_header.contains(UNGROUPED_EMOJI),
            "{ungrouped_header}"
        );
    }

    // --- truncation / age / cwd -----------------------------------------

    #[test]
    fn japanese_title_truncates_with_a_trailing_ellipsis_in_a_narrow_area() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let long_title = "\u{65e5}\u{672c}\u{8a9e}".repeat(10); // 日本語 x10
        let mut app = App::new(vec![row("j", &long_title, "", now)]);
        app.set_viewport_height(10);

        let text = draw_list(&app, 20, 10, now);
        assert!(text.contains('\u{2026}'), "expected an ellipsis:\n{text}");
        assert!(
            !text.contains(&long_title),
            "title should have been truncated:\n{text}"
        );
    }

    #[test]
    fn age_is_right_aligned_at_the_rows_right_edge() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let five_min_ago = now - Duration::from_secs(5 * 60);
        let mut app = App::new(vec![row("r", "Short", "", five_min_ago)]);
        app.set_viewport_height(10);

        // area width 40: prefix(8) + "Short"(5) = 13 used, "5m" (2 cols)
        // flush at the very end fills the row exactly (13 + 25-col gap + 2 =
        // 40) — the rendered line's last two characters are the age itself.
        let text = draw_list(&app, 40, 10, now);
        let line = text.lines().find(|l| l.contains("Short")).unwrap();
        assert!(line.ends_with("5m"), "age not right-aligned:\n{line:?}");
    }

    #[test]
    fn cwd_is_shown_and_leading_truncated_when_the_title_leaves_room() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let cwd = format!("/head_marker/{}/tail_marker", "x".repeat(80));
        let mut app = App::new(vec![row("r", "Short", &cwd, now)]);
        app.set_viewport_height(10);

        let text = draw_list(&app, 60, 10, now);
        let line = text.lines().find(|l| l.contains("Short")).unwrap();
        assert!(
            line.contains("Short"),
            "title should be shown verbatim, untruncated:\n{line}"
        );
        assert!(
            !line.contains("head_marker"),
            "cwd's head should have been cut:\n{line}"
        );
        assert!(
            line.contains("tail_marker"),
            "cwd's tail should survive (leading truncation):\n{line}"
        );
        assert!(
            line.contains('\u{2026}'),
            "expected a leading ellipsis:\n{line}"
        );
    }

    #[test]
    fn cwd_still_appears_at_and_above_the_width_floor() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let cwd = "/work/project";
        let mut app = App::new(vec![row("r", "Short", cwd, now)]);
        app.set_viewport_height(10);

        // Guards against over-suppression: MIN_WIDTH_FOR_CWD (60) is a
        // floor, not a ceiling — cwd must still show at and above it,
        // provided the title otherwise leaves it enough room.
        for width in [60, 80] {
            let text = draw_list(&app, width, 10, now);
            let line = text.lines().find(|l| l.contains("Short")).unwrap();
            assert!(
                line.contains("project"),
                "cwd should still show at width {width} (>= the floor):\n{line}"
            );
        }
    }

    #[test]
    fn cwd_never_shown_below_the_sidebar_width_floor_even_with_room() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let cwd = "/some/long/project/path";
        let mut app = App::new(vec![row("r", "Short", cwd, now)]);
        app.set_viewport_height(10);

        // 34 cols (the emporium sidebar's width) is below MIN_WIDTH_FOR_CWD
        // — the short title alone would ordinarily
        // leave more than enough room for cwd under the
        // title-leaves-room rule, but the floor overrides it here.
        let text = draw_list(&app, 34, 10, now);
        let line = text.lines().find(|l| l.contains("Short")).unwrap();
        assert!(
            !line.contains("project"),
            "cwd should never appear below the sidebar width floor:\n{line}"
        );
    }

    #[test]
    fn cwd_is_dropped_and_the_title_is_truncated_when_there_is_no_room() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let long_title = "This Is A Very Long Session Title Indeed";
        let mut app = App::new(vec![row("r", long_title, "/some/path", now)]);
        app.set_viewport_height(10);

        // Wide enough that "This Is" (the title's start) survives
        // truncation and can anchor the line lookup even with the agent
        // label's own width reserved alongside it, but still well under
        // MIN_WIDTH_FOR_CWD and short of the full title's length, so cwd
        // stays dropped and the title still gets truncated.
        let text = draw_list(&app, 40, 10, now);
        let line = text.lines().find(|l| l.contains("This Is")).unwrap();
        assert!(
            !line.contains("/some/path"),
            "cwd should have been dropped entirely:\n{line}"
        );
        assert!(
            line.contains('\u{2026}'),
            "title should be truncated with a trailing ellipsis:\n{line}"
        );
    }

    #[test]
    fn narrow_area_drops_the_age_column_without_panicking() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![row("r", "Title", "", now)]);
        app.set_viewport_height(10);

        // Width 5 is less than fixed_prefix(8) + min_gap(2) + age_width(3
        // for "now") even before a title is considered: degrades to
        // prefix + truncated title, age dropped entirely, no panic.
        let text = draw_list(&app, 5, 10, now);
        assert!(
            !text.contains("now"),
            "age should have been dropped entirely, not collided into the prefix:\n{text}"
        );
    }

    // --- agent label (chōba: same line; sidebar: second line) -----------

    #[test]
    fn choba_row_shows_the_agent_label_after_cwd_and_before_age_for_both_products() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![
            row("c", "Claude Row", "", now),
            codex_row("x", "Codex Row", "", now),
        ]);
        app.set_viewport_height(10);

        let text = draw_list(&app, 60, 10, now);

        let claude_line = text.lines().find(|l| l.contains("Claude Row")).unwrap();
        assert!(claude_line.contains("[Claude]"), "{claude_line}");
        let label_at = claude_line.find("[Claude]").unwrap();
        let age_at = claude_line.find("now").unwrap();
        assert!(
            label_at < age_at,
            "label should sit before age, not after:\n{claude_line}"
        );

        let codex_line = text.lines().find(|l| l.contains("Codex Row")).unwrap();
        assert!(codex_line.contains("[Codex]"), "{codex_line}");
    }

    #[test]
    fn choba_narrow_width_drops_the_label_and_age_together() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![row("r", "A Reasonably Long Title", "", now)]);
        app.set_viewport_height(10);

        // Too narrow for prefix(8) + label_gap(2) + "[Claude]"(8) +
        // min_gap(2) + age(3) all at once — both the label and age are
        // dropped together (the module's single narrow/normal split, not a
        // new three-tier cascade), leaving just the truncated title.
        let text = draw_list(&app, 15, 10, now);
        assert!(
            !text.contains("[Claude]"),
            "label should have been dropped:\n{text}"
        );
        assert!(
            !text.contains("now"),
            "age should have been dropped together with the label:\n{text}"
        );
        assert!(
            text.contains('\u{2026}'),
            "expected a truncated title:\n{text}"
        );
    }

    #[test]
    fn sidebar_row_carries_cwd_and_agent_label_on_a_second_line_for_both_products() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![
            row("c", "Claude Row", "/work/claude", now),
            codex_row("x", "Codex Row", "/work/codex", now),
        ])
        .with_lines_per_row(2);
        app.set_viewport_height(10);

        // 34 columns: the emporium sidebar's own inner width (36-column
        // SIDEBAR_WIDTH minus its 2-column border).
        let text = draw_list(&app, 34, 10, now);
        let lines: Vec<&str> = text.lines().collect();

        let claude_title = lines.iter().position(|l| l.contains("Claude Row")).unwrap();
        assert!(
            !lines[claude_title].contains("[Claude]"),
            "the label belongs on line two, not the title line:\n{}",
            lines[claude_title]
        );
        let claude_cwd_line = lines[claude_title + 1];
        assert!(claude_cwd_line.contains("claude"), "{claude_cwd_line}");
        assert!(claude_cwd_line.contains("[Claude]"), "{claude_cwd_line}");

        let codex_title = lines.iter().position(|l| l.contains("Codex Row")).unwrap();
        let codex_cwd_line = lines[codex_title + 1];
        assert!(codex_cwd_line.contains("codex"), "{codex_cwd_line}");
        assert!(codex_cwd_line.contains("[Codex]"), "{codex_cwd_line}");
    }

    #[test]
    fn sidebar_line_two_leading_truncates_cwd_and_keeps_the_label_flush_right() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let cwd = format!("/head_marker/{}/tail_marker", "x".repeat(40));
        let mut app = App::new(vec![row("r", "Short", &cwd, now)]).with_lines_per_row(2);
        app.set_viewport_height(10);

        let text = draw_list(&app, 34, 10, now);
        let lines: Vec<&str> = text.lines().collect();
        let title_line = lines.iter().position(|l| l.contains("Short")).unwrap();
        let cwd_line = lines[title_line + 1];

        assert!(!cwd_line.contains("head_marker"), "{cwd_line}");
        assert!(cwd_line.contains("tail_marker"), "{cwd_line}");
        assert!(
            cwd_line.contains('\u{2026}'),
            "expected a leading ellipsis:\n{cwd_line}"
        );
        assert!(
            cwd_line.trim_end().ends_with("[Claude]"),
            "label should be flush right:\n{cwd_line}"
        );
    }

    #[test]
    fn sidebar_line_two_truncates_the_label_itself_in_a_pathologically_narrow_sidebar() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![row("r", "Short", "/some/path", now)]).with_lines_per_row(2);
        app.set_viewport_height(10);

        // 6 columns is less than min_gap(2) + "[Claude]"(8) even before cwd
        // is considered — cwd is dropped entirely and even the label itself
        // is truncated, never panicking.
        let text = draw_list(&app, 6, 10, now);
        assert!(
            !text.contains('/'),
            "cwd should have been dropped entirely:\n{text}"
        );
    }

    #[test]
    fn sidebar_line_two_aligns_under_line_ones_title_in_both_flat_and_grouped_view() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut app = App::new(vec![
            row("a", "Alpha", "/work/a", now),
            row("b", "Beta", "/work/b", now),
        ])
        .with_pinned(["a".to_string()].into_iter().collect())
        .with_lines_per_row(2);
        app.set_viewport_height(10);

        let title_and_cwd_cols = |text: &str| -> (usize, usize) {
            let lines: Vec<&str> = text.lines().collect();
            let title_idx = lines
                .iter()
                .position(|l| l.contains("Alpha"))
                .unwrap_or_else(|| panic!("no row contains \"Alpha\":\n{text}"));
            let title_col = lines[title_idx].chars().position(|c| c == 'A').unwrap();
            let cwd_col = lines[title_idx + 1].chars().position(|c| c == '/').unwrap();
            (title_col, cwd_col)
        };

        // Grouped view (default): the pin slot is dropped (see
        // `grouped_view_drops_the_pin_column_so_titles_start_three_columns_earlier`),
        // so line two's indent must shrink by the same 3 columns as line
        // one's title — not stay at whatever flat view used.
        let (grouped_title_col, grouped_cwd_col) =
            title_and_cwd_cols(&draw_list(&app, 34, 10, now));
        assert_eq!(
            grouped_cwd_col, grouped_title_col,
            "line two should align under line one's title in grouped view"
        );

        app.toggle_grouped_view(); // flat: pin slot present
        let (flat_title_col, flat_cwd_col) = title_and_cwd_cols(&draw_list(&app, 34, 10, now));
        assert_eq!(
            flat_cwd_col, flat_title_col,
            "line two should align under line one's title in flat view"
        );

        assert_eq!(
            flat_title_col - grouped_title_col,
            3,
            "the two views should actually use different indents here, or this \
             test isn't exercising the thing it claims to"
        );
    }

    #[test]
    fn agent_label_color_differs_by_product_and_avoids_the_activity_dot() {
        let claude = agent_label_color(AgentKind::ClaudeCode);
        let codex = agent_label_color(AgentKind::Codex);
        assert_ne!(
            claude, codex,
            "the two products should be visually distinguishable"
        );

        // The doc above claims these stay clear of the activity dot's own
        // palette so the two coloured things in a row don't read as related.
        // Pin it: nothing else would notice a later edit colliding with the
        // dot until someone looked at a row and wondered why.
        for dot in [
            Color::Green,
            Color::Cyan,
            Color::Yellow,
            Color::Gray,
            Color::DarkGray,
        ] {
            assert_ne!(claude, dot, "agent label collides with an activity dot");
            assert_ne!(codex, dot, "agent label collides with an activity dot");
        }
    }
}
