//! What a staged tile says about its pane, drawn from a plain data struct the
//! shell assembles. Pure `(buffer, area, data)` like the other widgets here — it
//! reads nothing from the engine, so what it can show is exactly what
//! [`StripData`] carries.
//!
//! Two parts, one for each place a tile has room. The status — agent, activity
//! word, output age — is one row of the tile's TITLE, for every tile, collapsed
//! or not ([`title_status_parts`], [`render_title_status`]). The body of a
//! collapsed tile ([`render_strip`]) is the two rows under that title: the last
//! message the member was sent and the last it sent (`← 5m run the tests`,
//! `→ now done, see the diff`).
//! The colours are the session list's own (`view::activity_color`,
//! `view::agent_label_color`), shared rather than copied, so a tile and the
//! sidebar dot cannot drift apart.

use std::time::{Duration, SystemTime};

use banto_core::model::{Activity, AgentKind, humanize_age_compact};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use unicode_width::UnicodeWidthStr;

use crate::text::truncate_to_width;
use crate::view::{activity_color, activity_word, agent_label_color};

/// What a staged tile shows about its pane. Every field is optional: a member
/// with no session row yet may have no agent or activity, a pane that has not
/// written anything has no output age. The agent, activity and output age are the
/// title's status; the two messages are the collapsed body's rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StripData {
    pub agent: Option<AgentKind>,
    pub activity: Option<Activity>,
    /// Time since the pane last produced output.
    pub output_age: Option<Duration>,
    /// The newest message addressed to the member (first body row).
    pub inbound: Option<StripMessage>,
    /// The newest message the member sent (second body row).
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
/// so for it this is the only liveness signal on a tile.
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

/// The same age as a bare duration, in the same steps: "now", "20s", "3m",
/// "2h". What the title keeps of [`output_age_text`] when the long form no longer
/// fits.
pub fn output_age_short(age: Duration) -> String {
    let secs = age.as_secs();
    if secs < 10 {
        "now".to_string()
    } else if secs < 60 {
        format!("{}s", secs / 10 * 10)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

const SEPARATOR: &str = " \u{b7} ";

/// Draw the collapsed body's rows into `area` (the tile's content area: no
/// border): the last message received on the first row, the last sent on the
/// second. The status is not here; it is the title's. `now` is the clock the
/// messages' ages are measured against.
pub fn render_strip(buffer: &mut Buffer, area: Rect, data: &StripData, now: SystemTime) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    for (offset, arrow, message) in [(0, "\u{2190}", &data.inbound), (1, "\u{2192}", &data.sent)] {
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

/// One coloured piece of the title status.
type Part = Option<(String, Color)>;

/// How the output age is written at one step of the title status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgeForm {
    /// `output 2m ago` (`no output yet` for a pane that has written nothing).
    Long,
    /// `2m` beside a word; `output 2m` where there is no word, since a bare
    /// duration with nothing to say what it measures would not read.
    Short,
    /// Not shown.
    Left,
}

/// What each step of the title status keeps — agent, activity word, age in
/// which form — from the widest to the narrowest. The one place the shedding
/// order and the long/short age choice are written: the agent goes first, then
/// the age is shortened, then dropped; the word is never cut or shortened, and
/// the label (not here) never sheds. A step that keeps nothing, or that a wider
/// step already is (no age to shorten), is skipped.
///
/// Why the age is shortened before it is dropped: on a collapsed tile the pane
/// cannot be seen, and for Codex the output age is the only liveness signal, so
/// it has to outlive the agent label at the default collapsed width.
const STATUS_STEPS: [(bool, AgeForm); 4] = [
    (true, AgeForm::Long),
    (false, AgeForm::Long),
    (false, AgeForm::Short),
    (false, AgeForm::Left),
];

/// The status a tile's title row carries after its label, for every tile,
/// collapsed or not: agent label, activity word, output age, in the list's own
/// words and colours, from the same [`StripData`] — so one decision, drawn in
/// one place, whatever the tile's state.
///
/// Shed in [`STATUS_STEPS`] order as `room` cells run out: the agent label, then
/// the age's long form for its short one (`word · 2m`), then the age, and last
/// everything, never half a word. A pane with no activity known gets what is
/// left the same way (`Agent · output 2m ago`, `output 2m ago`, `output 2m`),
/// and nothing once not even that fits.
pub fn title_status_parts(data: &StripData, room: usize) -> Vec<(String, Color)> {
    let agent: Part = data
        .agent
        .map(|agent| (agent.label().to_string(), agent_label_color(agent)));
    let word: Part = data.activity.map(|activity| {
        (
            activity_word(activity).to_string(),
            activity_color(activity),
        )
    });
    for (with_agent, form) in STATUS_STEPS {
        let age: Part = match form {
            AgeForm::Long => Some((
                data.output_age
                    .map_or_else(|| "no output yet".to_string(), output_age_text),
                Color::Gray,
            )),
            AgeForm::Short => data.output_age.map(|age| {
                let short = output_age_short(age);
                (
                    if word.is_some() {
                        short
                    } else {
                        format!("output {short}")
                    },
                    Color::Gray,
                )
            }),
            AgeForm::Left => None,
        };
        let kept: Vec<&(String, Color)> = [(with_agent, &agent), (true, &word), (true, &age)]
            .into_iter()
            .filter(|(keep, _)| *keep)
            .filter_map(|(_, part)| part.as_ref())
            .collect();
        if kept.is_empty() {
            continue;
        }
        let width: usize = kept.iter().map(|part| part.0.width()).sum::<usize>()
            + SEPARATOR.width() * (kept.len() - 1);
        if width > room {
            continue;
        }
        let mut parts = Vec::new();
        for (index, (text, color)) in kept.into_iter().enumerate() {
            if index > 0 {
                parts.push((SEPARATOR.to_string(), Color::DarkGray));
            }
            parts.push((text.clone(), *color));
        }
        return parts;
    }
    Vec::new()
}

/// Draw [`title_status_parts`] on row `y` into the cells `from..to` (a free
/// stretch of a tile's top border), set off by a space on each side the way the
/// title's own text is. Draws nothing when there is no room for even the word.
/// Returns the number of cells it used.
pub fn render_title_status(
    buffer: &mut Buffer,
    y: u16,
    from: u16,
    to: u16,
    data: &StripData,
) -> u16 {
    let room = usize::from(to.saturating_sub(from)).saturating_sub(2);
    let parts = title_status_parts(data, room);
    if parts.is_empty() {
        return 0;
    }
    let mut x = from;
    buffer.set_string(x, y, " ", Style::default());
    x += 1;
    for (text, color) in &parts {
        buffer.set_string(x, y, text, Style::default().fg(*color));
        x += text.width() as u16;
    }
    buffer.set_string(x, y, " ", Style::default());
    x + 1 - from
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

    #[test]
    fn the_short_age_is_the_long_one_without_its_words_in_the_same_steps() {
        for secs in [
            0,
            9,
            10,
            19,
            20,
            59,
            60,
            179,
            3599,
            3600,
            3 * 3600 + 5,
            30 * 3600,
        ] {
            let age = Duration::from_secs(secs);
            let long = output_age_text(age);
            let short = output_age_short(age);
            // "output 2m ago" -> "2m"; "output now" -> "now".
            let bare = long
                .strip_prefix("output ")
                .unwrap()
                .strip_suffix(" ago")
                .map_or_else(
                    || long.strip_prefix("output ").unwrap().to_string(),
                    str::to_string,
                );
            assert_eq!(short, bare, "{secs}s");
        }
    }

    fn draw(width: u16, data: &StripData) -> Buffer {
        let area = Rect::new(0, 0, width, 2);
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
    fn the_strip_draws_only_the_two_message_rows_and_never_a_status() {
        let data = StripData {
            inbound: said(300, "run the tests please"),
            sent: said(5, "done, see the diff"),
            ..busy_claude()
        };
        let buffer = draw(40, &data);
        assert_eq!(
            row_text(&buffer, 0, 40).trim_end(),
            "\u{2190} 5m run the tests please"
        );
        assert_eq!(
            row_text(&buffer, 1, 40).trim_end(),
            "\u{2192} now done, see the diff"
        );
        // The arrow and age are dim, the message itself is plain.
        assert_eq!(buffer[(0, 0)].fg, Color::DarkGray);
        assert_eq!(buffer[(2, 0)].fg, Color::DarkGray);
        assert_eq!(buffer[(5, 0)].fg, Color::Reset);
        // Nothing of the agent, word or age anywhere in the area.
        let nothing = draw(40, &busy_claude());
        assert_eq!(row_text(&nothing, 0, 40).trim_end(), "");
        assert_eq!(row_text(&nothing, 1, 40).trim_end(), "");
    }

    #[test]
    fn a_row_with_no_message_is_blank_and_the_other_still_shows() {
        let only_sent = StripData {
            sent: said(7200, "shipped"),
            ..busy_claude()
        };
        let buffer = draw(40, &only_sent);
        assert_eq!(row_text(&buffer, 0, 40).trim_end(), "");
        assert_eq!(row_text(&buffer, 1, 40).trim_end(), "\u{2192} 2h shipped");
    }

    #[test]
    fn a_long_line_is_cut_with_an_ellipsis_by_display_width() {
        let data = StripData {
            inbound: said(0, "abcdefghijklmnopqrstuvwxyz"),
            ..busy_claude()
        };
        // "<- now " is 6 cells; 10 columns leave 4: three letters and the ellipsis.
        assert_eq!(
            row_text(&draw(10, &data), 0, 10),
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
        let line = row_text(&buffer, 0, 11);
        assert!(line.starts_with("\u{2190} now \u{30c6}"), "{line:?}");
        assert!(line.contains('\u{2026}'), "{line:?}");
        // A row narrower than the prefix itself clips the prefix and draws no text.
        assert_eq!(row_text(&draw(4, &wide), 0, 4), "\u{2190} no");
    }

    #[test]
    fn a_one_row_area_has_only_the_inbound_row() {
        let data = StripData {
            inbound: said(0, "hi"),
            sent: said(0, "yo"),
            ..busy_claude()
        };
        let area = Rect::new(0, 0, 30, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 3));
        render_strip(&mut buffer, area, &data, clock());
        assert_eq!(row_text(&buffer, 0, 30).trim_end(), "\u{2190} now hi");
        assert_eq!(row_text(&buffer, 1, 30).trim_end(), "");
        assert_eq!(row_text(&buffer, 2, 30).trim_end(), "");
    }

    #[test]
    fn an_empty_area_draws_nothing() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 3));
        let now = SystemTime::UNIX_EPOCH;
        let data = StripData {
            inbound: said(0, "hi"),
            ..busy_claude()
        };
        render_strip(&mut buffer, Rect::new(0, 0, 0, 3), &data, now);
        render_strip(&mut buffer, Rect::new(0, 0, 5, 0), &data, now);
        assert_eq!(row_text(&buffer, 0, 5).trim_end(), "");
    }

    fn texts(parts: &[(String, Color)]) -> String {
        parts.iter().map(|(text, _)| text.as_str()).collect()
    }

    fn pane(agent: AgentKind, activity: Activity, age: u64) -> StripData {
        StripData {
            agent: Some(agent),
            activity: Some(activity),
            output_age: Some(Duration::from_secs(age)),
            ..StripData::default()
        }
    }

    #[test]
    fn the_title_status_is_the_lists_words_and_colours() {
        let data = pane(AgentKind::Codex, Activity::Busy, 200);
        let parts = title_status_parts(&data, 80);
        assert_eq!(texts(&parts), "Codex \u{b7} busy \u{b7} output 3m ago");
        assert_eq!(parts[0].1, agent_label_color(AgentKind::Codex));
        assert_eq!(parts[2].1, activity_color(Activity::Busy));
        assert_eq!(parts[4].1, Color::Gray);
        // The short age keeps the colour of the long one.
        let short = title_status_parts(&data, 9);
        assert_eq!(texts(&short), "busy \u{b7} 3m");
        assert_eq!(short[2].1, Color::Gray);
    }

    /// The widths at which each step of the status is taken, for one pane, as
    /// `(room, what is shown)` pairs from the widest boundary down.
    #[test]
    fn the_title_status_sheds_the_agent_then_shortens_the_age_then_drops_it_and_never_cuts_the_word()
     {
        let data = pane(AgentKind::ClaudeCode, Activity::Waiting, 125);
        let full = "Claude \u{b7} waiting \u{b7} output 2m ago";
        assert_eq!(full.width(), 32);
        for (room, expected) in [
            (200, full),
            (32, full),
            (31, "waiting \u{b7} output 2m ago"),
            (23, "waiting \u{b7} output 2m ago"),
            (22, "waiting \u{b7} 2m"),
            (12, "waiting \u{b7} 2m"),
            (11, "waiting"),
            (7, "waiting"),
            (6, ""),
            (0, ""),
        ] {
            assert_eq!(
                texts(&title_status_parts(&data, room)),
                expected,
                "room {room}"
            );
        }
    }

    #[test]
    fn the_short_age_reads_now_when_the_pane_has_just_written() {
        let data = pane(AgentKind::ClaudeCode, Activity::Busy, 3);
        assert_eq!(texts(&title_status_parts(&data, 10)), "busy \u{b7} now");
        assert_eq!(texts(&title_status_parts(&data, 7)), "busy");
    }

    #[test]
    fn a_pane_with_no_age_has_no_short_form_to_keep() {
        let data = StripData {
            agent: Some(AgentKind::ClaudeCode),
            activity: Some(Activity::Alive),
            output_age: None,
            ..StripData::default()
        };
        assert_eq!(
            texts(&title_status_parts(&data, 80)),
            "Claude \u{b7} alive \u{b7} no output yet"
        );
        assert_eq!(
            texts(&title_status_parts(&data, 29)),
            "alive \u{b7} no output yet"
        );
        // Nothing to shorten: it goes to the bare word.
        assert_eq!(texts(&title_status_parts(&data, 20)), "alive");
    }

    #[test]
    fn a_pane_with_no_activity_known_shows_what_is_known() {
        let no_activity = StripData {
            agent: Some(AgentKind::Codex),
            activity: None,
            output_age: Some(Duration::from_secs(125)),
            ..StripData::default()
        };
        // Agent and age, then the age, then the age without "ago" (a bare "2m"
        // beside a label would not say what it measures), then nothing.
        for (room, expected) in [
            (80, "Codex \u{b7} output 2m ago"),
            (21, "Codex \u{b7} output 2m ago"),
            (20, "output 2m ago"),
            (13, "output 2m ago"),
            (12, "output 2m"),
            (9, "output 2m"),
            (8, ""),
        ] {
            assert_eq!(
                texts(&title_status_parts(&no_activity, room)),
                expected,
                "room {room}"
            );
        }
        let never = StripData {
            agent: Some(AgentKind::Codex),
            ..StripData::default()
        };
        assert_eq!(
            texts(&title_status_parts(&never, 80)),
            "Codex \u{b7} no output yet"
        );
        assert_eq!(texts(&title_status_parts(&never, 14)), "no output yet");
        assert_eq!(texts(&title_status_parts(&never, 12)), "");
        assert_eq!(
            texts(&title_status_parts(&StripData::default(), 80)),
            "no output yet"
        );
    }

    #[test]
    fn render_title_status_sets_the_text_off_with_a_space_each_side_and_reports_its_width() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 1));
        let used = render_title_status(
            &mut buffer,
            0,
            3,
            30,
            &pane(AgentKind::Codex, Activity::Busy, 0),
        );
        let drawn: String = (0..40).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(drawn.starts_with("   \u{20}Codex"), "{drawn:?}");
        assert_eq!(drawn.trim_end(), "    Codex \u{b7} busy \u{b7} output now");
        assert_eq!(used, " Codex \u{b7} busy \u{b7} output now ".width() as u16);
        // No room: nothing drawn.
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 1));
        assert_eq!(
            render_title_status(
                &mut buffer,
                0,
                3,
                8,
                &pane(AgentKind::Codex, Activity::Busy, 0)
            ),
            0
        );
        assert_eq!(buffer, Buffer::empty(Rect::new(0, 0, 40, 1)));
    }
}
