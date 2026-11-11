//! The body of a collapsed stack tile: a few rows of status-like facts, drawn
//! from a plain data struct the shell assembles. Pure `(buffer, area, data)`
//! like the other widgets here — it reads nothing from the engine, so what it
//! can show is exactly what [`StripData`] carries.
//!
//! The first content row is the status row: agent, activity word, output age.
//! The second and third are the last message the member was sent and the last
//! it sent (`← 5m run the tests`, `→ now done, see the diff`).
//! The colours are the session list's own (`view::activity_color`,
//! `view::agent_label_color`), shared rather than copied, so a collapsed tile
//! and the sidebar dot cannot drift apart.

use std::time::{Duration, SystemTime};

use banto_core::model::{Activity, AgentKind, humanize_age_compact};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use unicode_width::UnicodeWidthStr;

use crate::text::truncate_to_width;
use crate::view::{activity_color, activity_word, agent_label_color};

/// What a collapsed tile shows. Every field is optional: a member with no
/// session row yet has no agent or activity, a pane that has not written
/// anything has no output age.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StripData {
    pub agent: Option<AgentKind>,
    pub activity: Option<Activity>,
    /// Time since the pane last produced output.
    pub output_age: Option<Duration>,
    /// The newest message addressed to the member (row two).
    pub inbound: Option<StripMessage>,
    /// The newest message the member sent (row three).
    pub sent: Option<StripMessage>,
}

/// One message as a strip row shows it: when, and its first line (already
/// reduced where it left the store; this only clips it to the row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripMessage {
    pub at: SystemTime,
    pub text: String,
}

/// "output now", "output 20s ago" in ten-second steps up to a minute, then
/// "output 3m ago", then "output 2h ago". Codex never reports busy/waiting,
/// so for it this is the only liveness signal on the strip.
pub fn output_age_text(age: Duration) -> String {
    let secs = age.as_secs();
    if secs < 10 {
        "output now".to_string()
    } else if secs < 60 {
        format!("output {}s ago", secs / 10 * 10)
    } else if secs < 3600 {
        format!("output {}m ago", secs / 60)
    } else {
        format!("output {}h ago", secs / 3600)
    }
}

const SEPARATOR: &str = " \u{b7} ";

/// Draw the strip's rows into `area` (the tile's content area: no border).
/// `now` is the clock the messages' ages are measured against.
pub fn render_strip(buffer: &mut Buffer, area: Rect, data: &StripData, now: SystemTime) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    render_status_row(buffer, Rect { height: 1, ..area }, data);
    for (offset, arrow, message) in [(1, "\u{2190}", &data.inbound), (2, "\u{2192}", &data.sent)] {
        if offset < area.height
            && let Some(message) = message
        {
            let row = Rect {
                y: area.y + offset,
                height: 1,
                ..area
            };
            render_message_row(buffer, row, arrow, message, now);
        }
    }
}

/// `<arrow> <age> <first line>`, the line clipped by display width to what
/// is left of the row. A row with no message is left blank.
fn render_message_row(
    buffer: &mut Buffer,
    row: Rect,
    arrow: &str,
    message: &StripMessage,
    now: SystemTime,
) {
    let age = humanize_age_compact(message.at, now);
    let prefix = format!("{arrow} {age} ");
    let prefix_width = prefix.width() as u16;
    buffer.set_stringn(
        row.x,
        row.y,
        &prefix,
        usize::from(row.width),
        Style::default().fg(Color::DarkGray),
    );
    if prefix_width < row.width {
        let room = row.width - prefix_width;
        buffer.set_stringn(
            row.x + prefix_width,
            row.y,
            truncate_to_width(&message.text, room),
            usize::from(room),
            Style::default(),
        );
    }
}

/// One coloured piece of the status row.
type Part = Option<(String, Color)>;

/// Agent · activity · output age on one row. When the row is too narrow the
/// agent label goes first, then the activity word, never the output age
/// (which is cut to fit only as a last resort).
fn render_status_row(buffer: &mut Buffer, row: Rect, data: &StripData) {
    let age = data
        .output_age
        .map_or_else(|| "no output yet".to_string(), output_age_text);
    let mut agent: Part = data
        .agent
        .map(|agent| (agent.label().to_string(), agent_label_color(agent)));
    let mut activity: Part = data.activity.map(|activity| {
        (
            activity_word(activity).to_string(),
            activity_color(activity),
        )
    });
    let width = |agent: &Part, activity: &Part| {
        let mut total = age.width();
        for part in [agent, activity].into_iter().flatten() {
            total += part.0.width() + SEPARATOR.width();
        }
        total
    };
    if width(&agent, &activity) > usize::from(row.width) {
        agent = None;
    }
    if width(&agent, &activity) > usize::from(row.width) {
        activity = None;
    }
    let mut x = row.x;
    let end = row.x + row.width;
    let mut put = |text: &str, style: Style| {
        if x >= end {
            return;
        }
        let room = end - x;
        let text = truncate_to_width(text, room);
        buffer.set_stringn(x, row.y, &text, usize::from(room), style);
        x += text.width() as u16;
    };
    for (text, color) in [&agent, &activity].into_iter().flatten() {
        put(text, Style::default().fg(*color));
        put(SEPARATOR, Style::default().fg(Color::DarkGray));
    }
    put(&age, Style::default().fg(Color::Gray));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(buffer: &Buffer, y: u16, width: u16) -> String {
        (0..width).map(|x| buffer[(x, y)].symbol()).collect()
    }

    #[test]
    fn output_age_buckets() {
        let text = |secs| output_age_text(Duration::from_secs(secs));
        assert_eq!(text(0), "output now");
        assert_eq!(text(9), "output now");
        assert_eq!(text(10), "output 10s ago");
        assert_eq!(text(19), "output 10s ago");
        assert_eq!(text(20), "output 20s ago");
        assert_eq!(text(59), "output 50s ago");
        assert_eq!(text(60), "output 1m ago");
        assert_eq!(text(179), "output 2m ago");
        assert_eq!(text(3599), "output 59m ago");
        assert_eq!(text(3600), "output 1h ago");
        assert_eq!(text(3 * 3600 + 5), "output 3h ago");
        assert_eq!(text(30 * 3600), "output 30h ago");
    }

    fn draw(width: u16, data: &StripData) -> Buffer {
        let area = Rect::new(0, 0, width, 3);
        let mut buffer = Buffer::empty(area);
        render_strip(
            &mut buffer,
            area,
            data,
            SystemTime::UNIX_EPOCH + Duration::from_secs(10_000),
        );
        buffer
    }

    fn busy_claude() -> StripData {
        StripData {
            agent: Some(AgentKind::ClaudeCode),
            activity: Some(Activity::Busy),
            output_age: Some(Duration::from_secs(25)),
            ..StripData::default()
        }
    }

    #[test]
    fn the_status_row_names_agent_activity_and_output_age_in_the_sidebars_colours() {
        let buffer = draw(40, &busy_claude());
        assert_eq!(
            row_text(&buffer, 0, 40).trim_end(),
            "Claude \u{b7} busy \u{b7} output 20s ago"
        );
        assert_eq!(buffer[(0, 0)].fg, agent_label_color(AgentKind::ClaudeCode));
        assert_eq!(buffer[(9, 0)].fg, activity_color(Activity::Busy));
        assert_eq!(
            buffer[(9, 0)].fg,
            Color::Green,
            "busy is the sidebar's green"
        );
        // The other rows are left alone in part A.
        assert_eq!(row_text(&buffer, 1, 40).trim_end(), "");
    }

    #[test]
    fn a_waiting_session_reads_in_the_waiting_colour() {
        let data = StripData {
            activity: Some(Activity::Waiting),
            ..busy_claude()
        };
        let buffer = draw(40, &data);
        let text = row_text(&buffer, 0, 40);
        let at = text.find("waiting").unwrap() as u16;
        assert_eq!(buffer[(at, 0)].fg, crate::view::WAITING_ACTIVITY_COLOR);
    }

    #[test]
    fn a_narrow_row_drops_the_agent_first_then_the_activity_and_keeps_the_output_age() {
        let no_agent = "busy \u{b7} output 20s ago".chars().count() as u16;
        let buffer = draw(no_agent, &busy_claude());
        assert_eq!(row_text(&buffer, 0, no_agent), "busy \u{b7} output 20s ago");
        let buffer = draw(no_agent - 1, &busy_claude());
        assert_eq!(
            row_text(&buffer, 0, no_agent - 1).trim_end(),
            "output 20s ago",
            "then the activity word"
        );
        let buffer = draw(14, &busy_claude());
        assert_eq!(row_text(&buffer, 0, 14), "output 20s ago");
    }

    #[test]
    fn codex_has_no_activity_and_its_output_age_always_stays() {
        let data = StripData {
            agent: Some(AgentKind::Codex),
            activity: None,
            output_age: Some(Duration::from_secs(3)),
            ..StripData::default()
        };
        assert_eq!(
            row_text(&draw(40, &data), 0, 40).trim_end(),
            "Codex \u{b7} output now"
        );
        // Too narrow for the label: the age stays.
        assert_eq!(row_text(&draw(10, &data), 0, 10), "output now");
        // Narrower than the age itself: cut, not dropped.
        assert_eq!(row_text(&draw(6, &data), 0, 6), "outpu\u{2026}");
    }

    #[test]
    fn a_member_with_nothing_known_still_gets_an_honest_row() {
        let buffer = draw(30, &StripData::default());
        assert_eq!(row_text(&buffer, 0, 30).trim_end(), "no output yet");
    }

    /// The clock `draw` measures message ages against.
    fn clock() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(10_000)
    }

    fn said(secs_ago: u64, text: &str) -> Option<StripMessage> {
        Some(StripMessage {
            at: clock() - Duration::from_secs(secs_ago),
            text: text.to_string(),
        })
    }

    #[test]
    fn rows_two_and_three_show_the_last_message_in_and_out_with_their_ages() {
        let data = StripData {
            inbound: said(300, "run the tests please"),
            sent: said(5, "done, see the diff"),
            ..busy_claude()
        };
        let buffer = draw(40, &data);
        assert_eq!(
            row_text(&buffer, 1, 40).trim_end(),
            "\u{2190} 5m run the tests please"
        );
        assert_eq!(
            row_text(&buffer, 2, 40).trim_end(),
            "\u{2192} now done, see the diff"
        );
        // The arrow and age are dim, the message itself is plain.
        assert_eq!(buffer[(0, 1)].fg, Color::DarkGray);
        assert_eq!(buffer[(2, 1)].fg, Color::DarkGray);
        assert_eq!(buffer[(5, 1)].fg, Color::Reset);
    }

    #[test]
    fn a_row_with_no_message_is_blank_and_the_other_still_shows() {
        let only_sent = StripData {
            sent: said(7200, "shipped"),
            ..busy_claude()
        };
        let buffer = draw(40, &only_sent);
        assert_eq!(row_text(&buffer, 1, 40).trim_end(), "");
        assert_eq!(row_text(&buffer, 2, 40).trim_end(), "\u{2192} 2h shipped");
        let nothing = draw(40, &busy_claude());
        assert_eq!(row_text(&nothing, 1, 40).trim_end(), "");
        assert_eq!(row_text(&nothing, 2, 40).trim_end(), "");
    }

    #[test]
    fn a_long_line_is_cut_with_an_ellipsis_by_display_width() {
        let data = StripData {
            inbound: said(0, "abcdefghijklmnopqrstuvwxyz"),
            ..busy_claude()
        };
        // "<- now " is 6 cells; 10 columns leave 4: three letters and the ellipsis.
        assert_eq!(
            row_text(&draw(10, &data), 1, 10),
            "\u{2190} now abc\u{2026}"
        );
        // Wide characters count two cells and are never split: 5 cells left
        // fit two of them (4 cells) and the ellipsis would make 5, so one
        // character and the ellipsis (3 cells) is what fits in the rest.
        let wide = StripData {
            inbound: said(0, "\u{30c6}\u{30b9}\u{30c8}\u{30c6}\u{30b9}\u{30c8}"),
            ..busy_claude()
        };
        let buffer = draw(11, &wide);
        let line = row_text(&buffer, 1, 11);
        assert!(line.starts_with("\u{2190} now \u{30c6}"), "{line:?}");
        assert!(line.contains('\u{2026}'), "{line:?}");
        // A row narrower than the prefix itself clips the prefix and draws no text.
        assert_eq!(row_text(&draw(4, &wide), 1, 4), "\u{2190} no");
    }

    #[test]
    fn a_one_row_area_has_only_the_status_row() {
        let data = StripData {
            inbound: said(0, "hi"),
            sent: said(0, "yo"),
            ..busy_claude()
        };
        let area = Rect::new(0, 0, 30, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 3));
        render_strip(&mut buffer, area, &data, clock());
        assert_eq!(row_text(&buffer, 1, 30).trim_end(), "");
        assert_eq!(row_text(&buffer, 2, 30).trim_end(), "");
    }

    #[test]
    fn an_empty_area_draws_nothing() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 3));
        let now = SystemTime::UNIX_EPOCH;
        render_strip(&mut buffer, Rect::new(0, 0, 0, 3), &busy_claude(), now);
        render_strip(&mut buffer, Rect::new(0, 0, 5, 0), &busy_claude(), now);
        assert_eq!(row_text(&buffer, 0, 5).trim_end(), "");
    }
}
