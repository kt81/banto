//! What a staged tile says about its pane, drawn from a plain data struct the
//! shell assembles. Pure `(buffer, area, data)` like the other widgets here — it
//! reads nothing from the engine, so what it can show is exactly what
//! [`StripData`] carries.
//!
//! Two parts, one for each place a tile has room. The status — agent, activity
//! word — is one row of the tile's TITLE, for every tile, collapsed
//! or not ([`title_status_parts`], [`render_title_status`]). The body of a
//! collapsed tile ([`render_strip`]) is the two rows under that title: the last
//! message the member was sent and the last it sent (`← 5m run the tests`,
//! `→ now done, see the diff`).
//! The colours are the session list's own (`view::activity_color`,
//! `view::agent_label_color`), shared rather than copied, so a tile and the
//! sidebar dot cannot drift apart.

use std::time::SystemTime;

use banto_core::model::{Activity, AgentKind, humanize_age_compact};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use unicode_width::UnicodeWidthStr;

use crate::text::truncate_to_width;
use crate::view::{activity_color, activity_word, agent_label_color};

/// What a staged tile shows about its pane. Every field is optional: a member
/// with no session row yet may have no agent or activity. The agent and activity
/// are the title's status; the two messages are the collapsed body's rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StripData {
    pub agent: Option<AgentKind>,
    pub activity: Option<Activity>,
    /// The newest message addressed to the member (first body row).
    pub inbound: Option<StripMessage>,
    /// The newest message the member sent (second body row).
    pub sent: Option<StripMessage>,
    /// The Director's brigade has an open request for the operator's decision:
    /// the fixed tag ([`REQUEST_TAG`]) is drawn on the title row after the status
    /// ([`title_request_parts`]). Only a Director's tile has one.
    pub request: bool,
    /// The activity is idle only for want of a turn: a background command of the
    /// session is still running. Drawn as the mark [`BACKGROUND_MARK`] beside the
    /// word, and only beside a word.
    pub background: bool,
}

/// One message as a strip row shows it: when, and its first line (already
/// reduced where it left the store; this only clips it to the row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripMessage {
    pub at: SystemTime,
    pub text: String,
}

const SEPARATOR: &str = " \u{b7} ";

/// The mark a tile draws after `idle` while a background command of the session is
/// still running.
pub const BACKGROUND_MARK: &str = "bg";

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

/// The status a tile's title row carries, for every tile, collapsed or not:
/// agent label, then activity word, in the list's own words and colours (busy,
/// waiting, idle), from the same [`StripData`] — so one decision, drawn in one
/// place, whatever the tile's state. A session whose turn has ended with a
/// background command still running reads `idle · bg` ([`BACKGROUND_MARK`])
/// where the word is.
///
/// As `room` cells run out the agent label goes first (`idle`), then everything:
/// never half a word, and never the word without its mark. A pane with no
/// activity known shows the agent alone, and nothing once that does not fit; a
/// pane with neither shows nothing.
pub fn title_status_parts(data: &StripData, room: usize) -> Vec<(String, Color)> {
    status_ladder(data)
        .into_iter()
        .find(|parts| parts_width(parts) <= room)
        .unwrap_or_default()
}

/// Every step of the title status, widest first, each as its coloured parts with
/// the separators in place: with the agent, then without it. A step that keeps
/// nothing, or that the wider step already is (no agent to shed), is not there.
/// The one place the steps are built, so the status alone ([`title_status_parts`])
/// and the status beside a request ([`title_request_parts`]) shed the same way.
/// The mark is never a step of its own: it is kept exactly when the word is.
///
/// There is no output age here, on purpose. It was dropped at the operator's
/// request, and the reason once given for keeping it (that for Codex it was the
/// only liveness signal) lapsed when a Codex tile got a word from the pane's own
/// report; do not add it back for Codex (`docs/REQUIREMENTS.md`, decision 11).
fn status_ladder(data: &StripData) -> Vec<Vec<(String, Color)>> {
    let agent: Part = data
        .agent
        .map(|agent| (agent.label().to_string(), agent_label_color(agent)));
    let word: Part = data.activity.map(|activity| {
        (
            activity_word(activity).to_string(),
            activity_color(activity),
        )
    });
    // Grey, not busy's or waiting's colour: the word says the turn has ended, and
    // the mark says only that something is still running behind it.
    let mark: Part =
        (data.background && word.is_some()).then(|| (BACKGROUND_MARK.to_string(), Color::Gray));
    let mut ladder: Vec<Vec<(String, Color)>> = Vec::new();
    for with_agent in [true, false] {
        let mut parts: Vec<(String, Color)> = Vec::new();
        for part in [if with_agent { &agent } else { &None }, &word, &mark]
            .into_iter()
            .flatten()
        {
            if !parts.is_empty() {
                parts.push((SEPARATOR.to_string(), Color::DarkGray));
            }
            parts.push(part.clone());
        }
        if !parts.is_empty() && ladder.last() != Some(&parts) {
            ladder.push(parts);
        }
    }
    ladder
}

fn parts_width(parts: &[(String, Color)]) -> usize {
    parts.iter().map(|(text, _)| text.width()).sum()
}

/// The fixed tag a Director's tile and list row carry while its brigade waits on
/// the operator's decision. A constant: no agent-written text reaches the screen
/// (the question is in the Director's own pane), so it is the same on every row,
/// is never cut and has no ellipsis.
pub const REQUEST_TAG: &str = "\u{25b8} decision";

/// The title row's status with the request tag after it: `Claude · busy ·
/// ▸ decision`. The tag is whole or absent. As the room shrinks the status sheds
/// as it always does (the agent) with the tag kept, then the word goes and the tag
/// stands alone (`▸ decision`); and when the tag itself does not fit the status is
/// drawn as if there were no request, so the word is not lost to a tag that cannot
/// be drawn. The tag is in the waiting colour. With no request this is
/// [`title_status_parts`].
pub fn title_request_parts(data: &StripData, room: usize) -> Vec<(String, Color)> {
    if !data.request {
        return title_status_parts(data, room);
    }
    let tag = (REQUEST_TAG.to_string(), activity_color(Activity::Waiting));
    let tag_width = REQUEST_TAG.width();
    for mut status in status_ladder(data) {
        if parts_width(&status) + SEPARATOR.width() + tag_width <= room {
            status.push((SEPARATOR.to_string(), Color::DarkGray));
            status.push(tag);
            return status;
        }
    }
    if tag_width <= room {
        return vec![tag];
    }
    title_status_parts(data, room)
}

/// Draw [`title_request_parts`] on row `y` into the cells `from..to` (a free
/// stretch of a tile's top border), set off by a space on each side the way the
/// title's own text is, and ending at `to`: the status is anchored on the right,
/// so on every tile of a stack the word ends in the same column whatever the
/// label's width, and the spare border lies between the label and the status.
/// Draws nothing when there is no room for even the word. Returns the number of
/// cells it used.
pub fn render_title_status(
    buffer: &mut Buffer,
    y: u16,
    from: u16,
    to: u16,
    data: &StripData,
) -> u16 {
    let room = usize::from(to.saturating_sub(from)).saturating_sub(2);
    let parts = title_request_parts(data, room);
    if parts.is_empty() {
        return 0;
    }
    let used = parts_width(&parts) as u16 + 2;
    let mut x = to - used;
    buffer.set_string(x, y, " ", Style::default());
    x += 1;
    for (text, color) in &parts {
        buffer.set_string(x, y, text, Style::default().fg(*color));
        x += text.width() as u16;
    }
    buffer.set_string(x, y, " ", Style::default());
    used
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn row_text(buffer: &Buffer, y: u16, width: u16) -> String {
        (0..width).map(|x| buffer[(x, y)].symbol()).collect()
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

    fn pane(agent: AgentKind, activity: Activity) -> StripData {
        StripData {
            agent: Some(agent),
            activity: Some(activity),
            ..StripData::default()
        }
    }

    #[test]
    fn the_title_status_is_the_lists_words_and_colours() {
        let data = pane(AgentKind::Codex, Activity::Busy);
        let parts = title_status_parts(&data, 80);
        assert_eq!(texts(&parts), "Codex \u{b7} busy");
        assert_eq!(parts[0].1, agent_label_color(AgentKind::Codex));
        assert_eq!(parts[2].1, activity_color(Activity::Busy));
        // Never an output age, however the pane is doing: it left the title row.
        for activity in [Activity::Busy, Activity::Waiting, Activity::Alive] {
            let drawn = texts(&title_status_parts(&pane(AgentKind::Codex, activity), 80));
            assert!(
                !drawn.contains("output") && !drawn.contains("ago"),
                "{drawn}"
            );
        }
    }

    /// The widths at which each step of the status is taken, for one pane, as
    /// `(room, what is shown)` pairs from the widest boundary down.
    #[test]
    fn the_title_status_sheds_the_agent_and_then_everything_and_never_cuts_the_word() {
        let data = pane(AgentKind::ClaudeCode, Activity::Waiting);
        let full = "Claude \u{b7} waiting";
        assert_eq!(full.width(), 16);
        for (room, expected) in [
            (200, full),
            (16, full),
            (15, "waiting"),
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

    fn in_the_background() -> StripData {
        StripData {
            background: true,
            ..pane(AgentKind::ClaudeCode, Activity::Alive)
        }
    }

    #[test]
    fn a_background_mark_follows_the_word_and_sheds_only_with_it() {
        assert_eq!(BACKGROUND_MARK, "bg");
        let data = in_the_background();
        let full = "Claude \u{b7} idle \u{b7} bg";
        assert_eq!(full.width(), 18);
        for (room, expected) in [
            (200, full),
            (18, full),
            (17, "idle \u{b7} bg"),
            (9, "idle \u{b7} bg"),
            // Never the word without its mark, and never the mark alone.
            (8, ""),
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
    fn the_background_mark_is_grey_not_busys_or_waitings_colour() {
        let parts = title_status_parts(&in_the_background(), 80);
        let mark = parts.iter().find(|(text, _)| text == "bg").unwrap();
        assert_eq!(mark.1, Color::Gray);
        let word = parts.iter().find(|(text, _)| text == "idle").unwrap();
        assert_eq!(word.1, activity_color(Activity::Alive));
        assert_ne!(mark.1, activity_color(Activity::Busy));
        assert_ne!(mark.1, activity_color(Activity::Waiting));
    }

    #[test]
    fn a_mark_with_no_word_to_stand_beside_is_not_drawn() {
        let data = StripData {
            activity: None,
            ..in_the_background()
        };
        for room in [80, 30, 20, 12, 9, 6, 4] {
            let drawn = texts(&title_status_parts(&data, room));
            assert!(!drawn.contains("bg"), "room {room}: {drawn:?}");
        }
        // And a pane that is not in the background never draws it.
        let plain = pane(AgentKind::ClaudeCode, Activity::Alive);
        for room in [80, 30, 20, 12, 9, 4] {
            let drawn = texts(&title_status_parts(&plain, room));
            assert!(!drawn.contains("bg"), "room {room}: {drawn:?}");
        }
        assert_eq!(texts(&title_status_parts(&plain, 80)), "Claude \u{b7} idle");
    }

    #[test]
    fn with_a_request_the_mark_stays_with_the_word_through_every_step() {
        let data = StripData {
            request: true,
            ..in_the_background()
        };
        let at = |room| texts(&title_request_parts(&data, room));
        let tag = "\u{25b8} decision";
        assert_eq!(at(80), format!("Claude \u{b7} idle \u{b7} bg \u{b7} {tag}"));
        assert_eq!(at(31), format!("Claude \u{b7} idle \u{b7} bg \u{b7} {tag}"));
        assert_eq!(at(30), format!("idle \u{b7} bg \u{b7} {tag}"));
        assert_eq!(at(22), format!("idle \u{b7} bg \u{b7} {tag}"));
        // The word and its mark go together, the tag standing alone ...
        assert_eq!(at(21), tag);
        assert_eq!(at(10), tag);
        // ... and a tag that does not fit leaves the status as if no request.
        assert_eq!(at(9), "idle \u{b7} bg");
        assert_eq!(at(8), "");
        for room in 0..120 {
            let drawn = at(room);
            assert!(drawn.width() <= room, "room {room}: {drawn:?}");
            assert_eq!(
                drawn.contains("bg"),
                drawn.contains("idle"),
                "room {room}: the word and its mark go together: {drawn:?}"
            );
        }
    }

    // --- a Director's request tag on its title row -------------------------------

    fn director(request: bool) -> StripData {
        StripData {
            agent: Some(AgentKind::ClaudeCode),
            activity: Some(Activity::Busy),
            request,
            ..StripData::default()
        }
    }

    #[test]
    fn without_a_request_the_title_status_is_what_it_always_was() {
        for room in [60, 30, 20, 9, 4, 3] {
            assert_eq!(
                title_status_parts(&director(false), room),
                title_request_parts(&director(false), room),
                "{room}"
            );
        }
    }

    #[test]
    fn the_tag_is_a_fixed_word_and_the_status_sheds_around_it_agent_first() {
        assert_eq!(REQUEST_TAG, "\u{25b8} decision");
        let data = director(true);
        let at = |room| texts(&title_request_parts(&data, room));
        // The whole status, then the tag.
        assert_eq!(at(80), "Claude \u{b7} busy \u{b7} \u{25b8} decision");
        assert_eq!(at(26), "Claude \u{b7} busy \u{b7} \u{25b8} decision");
        // The agent goes, with the tag kept ...
        assert_eq!(at(25), "busy \u{b7} \u{25b8} decision");
        assert_eq!(at(17), "busy \u{b7} \u{25b8} decision");
        // ... then the word goes and the tag stands alone ...
        assert_eq!(at(16), "\u{25b8} decision");
        assert_eq!(at(10), "\u{25b8} decision");
        // ... and when the tag does not fit the status is drawn as if there were no
        // request, so the word is not lost to a tag that cannot be drawn.
        assert_eq!(at(9), texts(&title_status_parts(&data, 9)));
        assert_eq!(at(9), "busy");
        assert_eq!(at(4), "busy");
        assert_eq!(at(3), "");
    }

    #[test]
    fn the_tag_is_never_cut_and_never_has_an_ellipsis_at_any_width() {
        let data = director(true);
        for room in 0..120 {
            let drawn = texts(&title_request_parts(&data, room));
            assert!(drawn.width() <= room, "room {room}: {drawn:?}");
            assert!(!drawn.contains('\u{2026}'), "room {room}: {drawn:?}");
            // Whole or absent: never a piece of it.
            let marker = drawn.contains('\u{25b8}');
            assert_eq!(
                marker,
                drawn.contains(REQUEST_TAG),
                "room {room}: {drawn:?}"
            );
        }
    }

    #[test]
    fn the_tag_is_in_the_waiting_colour_and_a_pane_with_no_word_still_shows_it() {
        let parts = title_request_parts(&director(true), 60);
        let tag = parts.iter().find(|(text, _)| text == REQUEST_TAG).unwrap();
        assert_eq!(tag.1, activity_color(Activity::Waiting));
        let no_word = StripData {
            activity: None,
            ..director(true)
        };
        // The agent, then the tag; and the tag alone when the agent will not fit.
        assert_eq!(
            texts(&title_request_parts(&no_word, 60)),
            "Claude \u{b7} \u{25b8} decision"
        );
        assert_eq!(texts(&title_request_parts(&no_word, 18)), REQUEST_TAG);
    }

    #[test]
    fn a_collapsed_tile_body_never_carries_a_request_it_is_the_titles() {
        // The tag is on the title row, which a collapsed tile draws the same way, so
        // the body keeps its messages whatever the Director's brigade is waiting on.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let message = |text: &str| StripMessage {
            at: now,
            text: text.to_string(),
        };
        let mut data = director(true);
        data.inbound = Some(message("run the tests"));
        data.sent = Some(message("done, see the diff"));
        let area = Rect::new(0, 0, 40, 2);
        let mut buffer = Buffer::empty(area);
        render_strip(&mut buffer, area, &data, now);
        let row = |y: u16| -> String {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        assert!(row(0).contains("run the tests"), "{:?}", row(0));
        assert!(!row(0).contains('\u{25b8}') && !row(1).contains('\u{25b8}'));
    }

    #[test]
    fn a_pane_with_no_activity_known_shows_its_agent_and_then_nothing() {
        let no_activity = StripData {
            agent: Some(AgentKind::Codex),
            activity: None,
            ..StripData::default()
        };
        for (room, expected) in [(80, "Codex"), (5, "Codex"), (4, ""), (0, "")] {
            assert_eq!(
                texts(&title_status_parts(&no_activity, room)),
                expected,
                "room {room}"
            );
        }
    }

    #[test]
    fn a_pane_with_neither_agent_nor_word_shows_nothing() {
        for room in [0, 5, 80] {
            assert!(title_status_parts(&StripData::default(), room).is_empty());
        }
        // A word with no agent known is still a word.
        let no_agent = StripData {
            activity: Some(Activity::Busy),
            ..StripData::default()
        };
        assert_eq!(texts(&title_status_parts(&no_agent, 80)), "busy");
    }

    #[test]
    fn render_title_status_is_anchored_on_the_right_with_a_space_each_side() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 1));
        let used = render_title_status(
            &mut buffer,
            0,
            3,
            30,
            &pane(AgentKind::Codex, Activity::Busy),
        );
        let drawn: String = (0..40).map(|x| buffer[(x, 0)].symbol()).collect();
        // `Codex · busy` is 12 cells; with its two spaces it ends at column 30
        // (exclusive), whatever the room to its left.
        assert_eq!(used, " Codex \u{b7} busy ".width() as u16);
        assert_eq!(
            drawn.trim_end(),
            format!("{}Codex \u{b7} busy", " ".repeat(17))
        );
        assert_eq!(buffer[(29, 0)].symbol(), " ");
        assert_eq!(buffer[(16, 0)].symbol(), " ");
        assert_eq!(buffer[(17, 0)].symbol(), "C");
        // The same status in a wider stretch ends in the same column.
        let mut wider = Buffer::empty(Rect::new(0, 0, 40, 1));
        render_title_status(
            &mut wider,
            0,
            3,
            30,
            &pane(AgentKind::Codex, Activity::Busy),
        );
        assert_eq!(wider, buffer);
        let mut other_from = Buffer::empty(Rect::new(0, 0, 40, 1));
        render_title_status(
            &mut other_from,
            0,
            12,
            30,
            &pane(AgentKind::Codex, Activity::Busy),
        );
        assert_eq!(
            other_from, buffer,
            "the word does not depend on where the label ends"
        );
        // No room: nothing drawn.
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 1));
        assert_eq!(
            render_title_status(
                &mut buffer,
                0,
                3,
                8,
                &pane(AgentKind::Codex, Activity::Busy)
            ),
            0
        );
        assert_eq!(buffer, Buffer::empty(Rect::new(0, 0, 40, 1)));
    }
}
