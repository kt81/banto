//! Shared session-list / summary rendering, used by both the classic list TUI
//! (`crate::tui`) and the emporium sidebar (`crate::embedded::emporium`).
//!
//! These are pure `(frame, app, area)` widgets so either mode can place them in
//! its own layout; all list/selection/scroll state lives in `crate::app::App`,
//! which both modes drive. Extracted from `crate::tui` so the two modes render
//! sessions identically rather than drifting.

use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use banto_core::model::{Activity, AgeBucket};

use crate::app::{App, ListLine};
use crate::session;

/// Render the session list (or a placeholder when nothing matches) into `area`.
pub(crate) fn render_list(frame: &mut Frame, app: &App, area: Rect) {
    if app.filtered_len() == 0 {
        let message = if app.total_len() == 0 {
            "No sessions found."
        } else {
            "No matching sessions."
        };
        let placeholder = Paragraph::new(message).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(placeholder, area);
        return;
    }

    let items: Vec<ListItem> = app.visible().into_iter().map(list_item).collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    state.select(app.selected_in_viewport());
    frame.render_stateful_widget(list, area, &mut state);
}

/// Build one list line: a bold section-header line (grouped view only), or a
/// row — colored activity dot, pin marker (if pinned), title (or id), dimmed
/// cwd. Each is its own `ListItem`/physical line rather than a header bundled
/// into its row, matching the index space `App::click`/`App::scroll`/
/// `App::ensure_visible` all use — see [`crate::app::ListLine`] for why that
/// matters for mouse clicks.
fn list_item(line: ListLine<'_>) -> ListItem<'static> {
    match line {
        ListLine::Header(name) => ListItem::new(Line::from(Span::styled(
            name,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))),
        ListLine::Row(visible) => {
            let dot = Span::styled(
                "\u{25cf} ",
                Style::default().fg(activity_color(visible.row.activity)),
            );
            let pin = if visible.pinned {
                // Plain ASCII, not a star symbol/emoji: those can render
                // double-width in some terminals and would break column
                // alignment.
                Span::styled(
                    "* ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("  ")
            };
            let director = if visible.director {
                Span::styled(
                    "D ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("  ")
            };
            let title = Span::raw(visible.row.display_title().to_string());
            let cwd = visible.row.cwd_display();
            let row_line = if cwd.is_empty() {
                Line::from(vec![dot, pin, director, title])
            } else {
                Line::from(vec![
                    dot,
                    pin,
                    director,
                    title,
                    Span::raw("  "),
                    Span::styled(cwd, Style::default().fg(Color::DarkGray)),
                ])
            };
            ListItem::new(row_line)
        }
    }
}

/// Render the always-visible summary panel: the selected session's activity
/// dot + title, preview excerpt, cwd, and a meta line (relative age, size,
/// short id, pinned/agent markers). A top border is the only visual
/// separation. A zero-height `area` (a too-short terminal, per the caller's
/// layout) makes this a no-op.
pub(crate) fn render_summary(frame: &mut Frame, app: &App, area: Rect) {
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

    let title_line = Line::from(vec![
        Span::styled(
            "\u{25cf} ",
            Style::default().fg(activity_color(row.activity)),
        ),
        Span::styled(
            row.display_title().to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]);
    let preview_line = Line::from(Span::styled(
        row.preview.as_deref().unwrap_or_default(),
        Style::default().fg(Color::DarkGray),
    ));
    let cwd_line = Line::from(Span::styled(
        row.cwd_display(),
        Style::default().fg(Color::DarkGray),
    ));
    let meta_line = Line::from(Span::styled(
        summary_meta(
            row,
            app.is_selected_pinned(),
            app.is_selected_director(),
            SystemTime::now(),
        ),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(
        Paragraph::new(vec![title_line, preview_line, cwd_line, meta_line]),
        inner,
    );
}

/// Build the summary panel's meta line: relative age, size, short id, and any
/// markers (pinned/director/agent) that apply.
fn summary_meta(
    row: &session::SessionRow,
    pinned: bool,
    director: bool,
    now: SystemTime,
) -> String {
    let mut parts = vec![
        session::humanize_age(row.mtime, now),
        session::humanize_size(row.size),
        session::short_id(&row.id),
    ];
    if pinned {
        parts.push("pinned".to_string());
    }
    if director {
        parts.push("director".to_string());
    }
    if row.is_agent {
        parts.push("agent".to_string());
    }
    parts.join("  \u{b7}  ")
}

/// Map an [`Activity`] to its list-dot color.
fn activity_color(activity: Activity) -> Color {
    match activity {
        Activity::Busy => Color::Green,
        Activity::Alive => Color::Cyan,
        Activity::Idle(AgeBucket::Today) => Color::Yellow,
        Activity::Idle(AgeBucket::ThisWeek) => Color::Gray,
        Activity::Idle(AgeBucket::Older) => Color::DarkGray,
    }
}
