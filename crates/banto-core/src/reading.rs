//! The reading screen: what a brigade's Director has asked the operator and how
//! far its work is, drawn full-size and read-only on F5.
//!
//! The geometry and the wrapped lines live here, in the core, and not in the
//! widget that draws them: a scroll position is only meaningful against the
//! number of lines the text wraps to, so the key handler has to know that
//! number to stop at the end, and the one place that can compute it for both is
//! a pure function of (state, width). The widget (`banto-tui`'s `render_modal`)
//! only maps a [`Tone`] to a colour.
//!
//! Nothing here reads a clock or the store. The body arrives as an `Event`
//! answering `StoreIntent::ReadBrigadeReading`, with ages as the shell that read
//! the rows computed them; the age shown while the screen stays open is that age
//! plus the time since the read.

use std::time::{Duration, Instant, SystemTime};

use ratatui_core::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::display_text::display_lines;
use crate::model::{BrigadeId, humanize_age};
use crate::plan::{PlanGlance, PlanReading, PlanState, PlanVersion, VersionKind};
use crate::progress::{BrigadeReading, TaskState};

/// The screen never grows past this many columns: a line of prose longer than
/// this is harder to read, not easier, and a Director's one-line fields are
/// bounded well below it.
const MAX_WIDTH: u16 = 100;
/// Margin kept around the screen, rows and columns.
const MARGIN: u16 = 2;

/// Where the screen sits in `full`: nearly all of it, capped in width. Larger
/// than the dialogs' box on purpose: its content is prose that scrolls, and a
/// full list of tasks should not need paging on a tall terminal.
pub fn reading_area(full: Rect) -> Rect {
    let width = full
        .width
        .saturating_sub(MARGIN * 2)
        .clamp(1, MAX_WIDTH.max(1));
    let height = full.height.saturating_sub(MARGIN * 2).max(1);
    let x = full.x + full.width.saturating_sub(width) / 2;
    let y = full.y + full.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

/// The columns and rows of text inside [`reading_area`]: the border takes one
/// cell on every side and the text is padded by one more column on the left and
/// right, as every dialog's content is.
pub fn reading_text_size(full: Rect) -> (usize, usize) {
    let area = reading_area(full);
    (
        usize::from(area.width).saturating_sub(4).max(1),
        usize::from(area.height).saturating_sub(2).max(1),
    )
}

/// What the body of the screen holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadingBody {
    /// Asked for, not answered yet.
    Loading,
    /// The store's answer, and when this process got it.
    Loaded {
        reading: BrigadeReading,
        read_at: Instant,
    },
    /// The store could not be read; the text is the reason, as the shell gave it.
    Unreadable(String),
}

/// What the tick last read about a brigade that makes the body stale when it
/// changes: the open request's sequence number, the Progress write count, and the
/// open Plan's glance (its newest version, its state, whether that version is a
/// draft). A change of any of them is a new read of the body, and only while the
/// screen is open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadingTrigger {
    pub request: Option<i64>,
    pub progress: Option<i64>,
    pub plan: Option<PlanGlance>,
}

/// The open reading screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadingState {
    brigade_id: BrigadeId,
    name: String,
    scroll: usize,
    body: ReadingBody,
    /// What the body was last asked for: when the tick sees a different pair, the
    /// body is asked for again. A new section of the screen (a Plan) adds a
    /// member here.
    asked: ReadingTrigger,
}

impl ReadingState {
    pub fn new(brigade_id: BrigadeId, name: String, asked: ReadingTrigger) -> Self {
        Self {
            brigade_id,
            name,
            scroll: 0,
            body: ReadingBody::Loading,
            asked,
        }
    }

    pub fn brigade_id(&self) -> BrigadeId {
        self.brigade_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn body(&self) -> &ReadingBody {
        &self.body
    }

    pub fn asked(&self) -> &ReadingTrigger {
        &self.asked
    }

    pub fn set_asked(&mut self, asked: ReadingTrigger) {
        self.asked = asked;
    }

    pub fn set_scroll(&mut self, scroll: usize) {
        self.scroll = scroll;
    }

    /// Replace the body with the store's answer. The scroll position stays: a
    /// refresh while the operator is reading must not throw them to the top.
    pub fn set_loaded(&mut self, reading: BrigadeReading, read_at: Instant) {
        self.body = ReadingBody::Loaded { reading, read_at };
    }

    pub fn set_unreadable(&mut self, reason: String) {
        self.body = ReadingBody::Unreadable(reason);
    }

    /// The Plan's number and the version the approval key approves, when the body
    /// on screen shows a draft: the key approves exactly what is displayed, so it
    /// is read from the body and never from the tick.
    pub fn approvable(&self) -> Option<(i64, i64)> {
        match &self.body {
            ReadingBody::Loaded { reading, .. } => {
                reading.plan.as_ref().and_then(|plan| plan.approvable())
            }
            _ => None,
        }
    }

    /// Whether the body says a decision is open, which is what Enter on the
    /// screen acts on.
    pub fn decision_open(&self) -> bool {
        matches!(
            &self.body,
            ReadingBody::Loaded { reading, .. } if reading.decision.is_some()
        )
    }
}

/// How a run of text is drawn. The widget maps it to a colour; the core names
/// the meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    /// A section's title.
    Heading,
    /// Something said about the text, not the text.
    Dim,
    /// A read that went wrong.
    Warn,
    /// A task's state word.
    Task(TaskState),
}

/// One line of the screen, as runs of text with the tone each is drawn in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadingLine {
    pub spans: Vec<(String, Tone)>,
}

impl ReadingLine {
    fn blank() -> Self {
        Self { spans: Vec::new() }
    }

    fn one(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            spans: vec![(text.into(), tone)],
        }
    }

    /// The line's text without its tones, for the tests.
    pub fn plain(&self) -> String {
        self.spans.iter().map(|(text, _)| text.as_str()).collect()
    }
}

/// Break `text` into lines no wider than `width` columns, by display width (a
/// Japanese character is two). Existing line breaks are kept; within a line the
/// break goes at the last space that fits, else inside the word. A space that
/// would leave nothing before the break (an indent) is not a break, and no line
/// is wider than `width` unless a single character is. A character wider than the
/// room still gets a line to itself rather than none.
pub fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0usize;
        // Byte offset of the last space in `line`, where a break reads best.
        let mut last_space: Option<usize> = None;
        for c in raw.chars() {
            let c_width = c.width().unwrap_or(0);
            let mut at_break = false;
            // A loop, not an `if`: the tail kept after a break at a space can itself
            // be too wide for `c` to join (a wide character after an odd remainder).
            while line_width + c_width > width && !line.is_empty() {
                match last_space {
                    Some(at) if c != ' ' && !line[..at].trim().is_empty() => {
                        let tail = line[at + 1..].to_string();
                        line.truncate(at);
                        out.push(line.trim_end().to_string());
                        line = tail;
                    }
                    _ => {
                        out.push(std::mem::take(&mut line));
                    }
                }
                line_width = line.width();
                last_space = None;
                if c == ' ' {
                    // A space that lands at a break is the break.
                    at_break = true;
                    break;
                }
            }
            if at_break {
                continue;
            }
            if c == ' ' {
                last_space = Some(line.len());
            }
            line.push(c);
            line_width += c_width;
        }
        out.push(line);
    }
    out
}

/// The least room worth hanging a text beside its lead. Below it the lead gets its
/// own line and the text follows indented: a hanging indent as wide as the window
/// would push every character of the text past the edge, where the renderer clips
/// it and no scrolling can bring it back.
const MIN_HANG_ROOM: usize = 12;

/// How far a text is indented under a lead that has a line to itself.
const STACKED_INDENT: usize = 2;

/// `spans` broken into lines no wider than `width` by display width, each piece
/// keeping its tone. The break is by character: a lead is an id and a state word,
/// not prose. A gap that would start a line of its own is dropped, since nothing
/// follows it once the lead has lines to itself.
fn wrap_spans(spans: Vec<(String, Tone)>, width: usize) -> Vec<Vec<(String, Tone)>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<(String, Tone)>> = vec![Vec::new()];
    let mut line_width = 0usize;
    for (text, tone) in spans {
        for c in text.chars() {
            let c_width = c.width().unwrap_or(0);
            if line_width + c_width > width && line_width > 0 {
                lines.push(Vec::new());
                line_width = 0;
            }
            let line = lines.last_mut().expect("a line is always open");
            match line.last_mut() {
                Some((run, run_tone)) if *run_tone == tone => run.push(c),
                _ => line.push((c.to_string(), tone)),
            }
            line_width += c_width;
        }
    }
    if let Some(last) = lines.last_mut() {
        while let Some((run, _)) = last.last_mut() {
            let kept = run.trim_end().len();
            run.truncate(kept);
            if run.is_empty() {
                last.pop();
            } else {
                break;
            }
        }
    }
    if lines.len() > 1 && lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    lines
}

/// `lead` (drawn once, in its tones) followed by `text` wrapped into the room
/// the lead leaves, the continuation lines indented under the text. When the lead
/// leaves less than [`MIN_HANG_ROOM`], the lead is a line of its own and the text
/// is wrapped beneath it.
fn hanging(
    lines: &mut Vec<ReadingLine>,
    lead: Vec<(String, Tone)>,
    text: &str,
    tone: Tone,
    width: usize,
) {
    let lead_width: usize = lead.iter().map(|(t, _)| t.width()).sum();
    if !lead.is_empty() && width.saturating_sub(lead_width) < MIN_HANG_ROOM {
        for spans in wrap_spans(lead, width) {
            lines.push(ReadingLine { spans });
        }
        if text.is_empty() {
            return;
        }
        // The indent gives way before a wide character does: a note that starts at
        // the indent with less than two columns left would be clipped whole.
        let indent = STACKED_INDENT.min(width.saturating_sub(2));
        for piece in wrap_to_width(text, width.saturating_sub(indent).max(1)) {
            let mut spans = vec![(" ".repeat(indent), Tone::Plain)];
            if !piece.is_empty() {
                spans.push((piece, tone));
            }
            lines.push(ReadingLine { spans });
        }
        return;
    }
    let room = width.saturating_sub(lead_width).max(1);
    let wrapped = wrap_to_width(text, room);
    for (i, piece) in wrapped.into_iter().enumerate() {
        let mut spans = if i == 0 {
            lead.clone()
        } else {
            vec![(" ".repeat(lead_width), Tone::Plain)]
        };
        if !piece.is_empty() {
            spans.push((piece, tone));
        }
        lines.push(ReadingLine { spans });
    }
}

/// `ms` as "5m ago", counted from a moment that is `elapsed` old.
fn age_phrase(age_ms: i64, elapsed: Duration) -> String {
    let total = Duration::from_millis(u64::try_from(age_ms.max(0)).unwrap_or(0)) + elapsed;
    humanize_age(SystemTime::UNIX_EPOCH, SystemTime::UNIX_EPOCH + total)
}

/// Every line of the screen at `width` columns, `now` being the moment the ages
/// are told as of.
pub fn reading_lines(state: &ReadingState, now: Instant, width: usize) -> Vec<ReadingLine> {
    let mut lines = Vec::new();
    match &state.body {
        ReadingBody::Loading => lines.push(ReadingLine::one("Reading\u{2026}", Tone::Dim)),
        ReadingBody::Unreadable(reason) => {
            hanging(
                &mut lines,
                vec![("Could not read this brigade: ".to_string(), Tone::Warn)],
                &display_lines(reason),
                Tone::Warn,
                width,
            );
        }
        ReadingBody::Loaded { reading, read_at } => {
            let elapsed = now.saturating_duration_since(*read_at);
            lines.push(ReadingLine::one("Decision", Tone::Heading));
            match &reading.decision {
                None => lines.push(ReadingLine::one("No decision is open.", Tone::Dim)),
                Some(decision) => {
                    if let Some(age_ms) = decision.age_ms {
                        lines.push(ReadingLine::one(
                            format!("asked {}", age_phrase(age_ms, elapsed)),
                            Tone::Dim,
                        ));
                    }
                    match &decision.question {
                        Some(question) => {
                            hanging(&mut lines, Vec::new(), question, Tone::Plain, width)
                        }
                        None => hanging(
                            &mut lines,
                            Vec::new(),
                            "The Director did not send its question here; it is in the \
                             Director's own pane.",
                            Tone::Dim,
                            width,
                        ),
                    }
                }
            }
            lines.push(ReadingLine::blank());
            lines.push(ReadingLine::one("Progress", Tone::Heading));
            match &reading.progress {
                None => lines.push(ReadingLine::one("None written yet.", Tone::Dim)),
                Some(record) => {
                    lines.push(ReadingLine::one(
                        format!("written {}", age_phrase(record.age_ms, elapsed)),
                        Tone::Dim,
                    ));
                    hanging(
                        &mut lines,
                        vec![("Stage  ".to_string(), Tone::Dim)],
                        &record.progress.stage,
                        Tone::Plain,
                        width,
                    );
                    if !record.progress.tasks.is_empty() {
                        lines.push(ReadingLine::blank());
                        lines.push(ReadingLine::one("Tasks", Tone::Heading));
                        for task in &record.progress.tasks {
                            // The state word is padded to the longest one, so the
                            // ids line up in a column.
                            let word = format!("{:<7}", task.state.as_token());
                            hanging(
                                &mut lines,
                                vec![
                                    ("  ".to_string(), Tone::Plain),
                                    (word, Tone::Task(task.state)),
                                    (" ".to_string(), Tone::Plain),
                                    (format!("{}  ", task.id), Tone::Dim),
                                ],
                                &task.note,
                                Tone::Plain,
                                width,
                            );
                        }
                    }
                    if !record.progress.off_plan.is_empty() {
                        lines.push(ReadingLine::blank());
                        lines.push(ReadingLine::one("Off plan", Tone::Heading));
                        for item in &record.progress.off_plan {
                            hanging(
                                &mut lines,
                                vec![("  \u{2022} ".to_string(), Tone::Dim)],
                                item,
                                Tone::Plain,
                                width,
                            );
                        }
                    }
                }
            }
            lines.push(ReadingLine::blank());
            lines.push(ReadingLine::one("Plan", Tone::Heading));
            plan_lines(&mut lines, reading.plan.as_deref(), elapsed, width);
        }
    }
    lines
}

/// The Plan section: which Plan and where it stands, then the draft awaiting the
/// operator (what the approval key approves, so it comes first), then the version
/// in force. Each version is its summary and its body wrapped by display width.
fn plan_lines(
    lines: &mut Vec<ReadingLine>,
    plan: Option<&PlanReading>,
    elapsed: Duration,
    width: usize,
) {
    let Some(plan) = plan else {
        lines.push(ReadingLine::one("No Plan is open.", Tone::Dim));
        return;
    };
    let view = &plan.view;
    let state = match view.plan.state {
        PlanState::Drafting => "drafting, nothing approved yet",
        PlanState::Approved => "approved",
        PlanState::Closed => "closed",
    };
    hanging(
        lines,
        Vec::new(),
        &format!("Plan {} \u{b7} {state}", view.plan.number),
        Tone::Dim,
        width,
    );
    hanging(lines, Vec::new(), &view.plan.title, Tone::Plain, width);
    let age_of = |at_ms: i64| age_phrase((plan.read_at_ms - at_ms).max(0), elapsed);
    if let Some(draft) = &view.pending {
        lines.push(ReadingLine::blank());
        hanging(
            lines,
            Vec::new(),
            &format!(
                "Draft, version {} \u{2014} awaits your approval",
                draft.version
            ),
            Tone::Heading,
            width,
        );
        hanging(
            lines,
            Vec::new(),
            &format!("written {}", age_of(draft.created_at_ms)),
            Tone::Dim,
            width,
        );
        version_lines(lines, draft, width);
    }
    if let Some(version) = &view.in_force {
        lines.push(ReadingLine::blank());
        hanging(
            lines,
            Vec::new(),
            &format!("In force, version {}", version.version),
            Tone::Heading,
            width,
        );
        let how = match (version.kind, version.approved_at_ms) {
            (VersionKind::PlannerReleased, _) => {
                format!("released by the Planner {}", age_of(version.created_at_ms))
            }
            (_, Some(at)) => format!("approved {}", age_of(at)),
            (_, None) => format!("written {}", age_of(version.created_at_ms)),
        };
        hanging(lines, Vec::new(), &how, Tone::Dim, width);
        version_lines(lines, version, width);
    }
}

/// A version's summary, then its body.
fn version_lines(lines: &mut Vec<ReadingLine>, version: &PlanVersion, width: usize) {
    hanging(lines, Vec::new(), &version.summary, Tone::Dim, width);
    lines.push(ReadingLine::blank());
    for piece in wrap_to_width(&version.body, width) {
        lines.push(ReadingLine::one(piece, Tone::Plain));
    }
}

/// The furthest the screen can scroll in a terminal of `size`: the number of
/// lines past the ones that fit.
pub fn reading_max_scroll(state: &ReadingState, now: Instant, size: (u16, u16)) -> usize {
    let (width, height) = reading_text_size(Rect::new(0, 0, size.0, size.1));
    reading_lines(state, now, width)
        .len()
        .saturating_sub(height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{DecisionReading, Progress, ProgressReading, ProgressTask};

    /// The one clock read the tests need: time is an argument everywhere else
    /// (`docs/DISCIPLINE.md` section 3), and these only need some instant to pass in.
    #[allow(clippy::disallowed_methods)]
    fn now() -> Instant {
        Instant::now()
    }

    fn loaded(reading: BrigadeReading) -> ReadingState {
        let mut state = ReadingState::new(1, "alpha".to_string(), ReadingTrigger::default());
        state.set_loaded(reading, now());
        state
    }

    fn texts(lines: &[ReadingLine]) -> Vec<String> {
        lines.iter().map(ReadingLine::plain).collect()
    }

    fn sample() -> BrigadeReading {
        BrigadeReading {
            plan: None,
            decision: Some(DecisionReading {
                seq: 3,
                question: Some("Ship it now?\nOr wait for the review.".to_string()),
                age_ms: Some(5 * 60 * 1000),
            }),
            progress: Some(ProgressReading {
                progress: Progress {
                    stage: "implementing the store".to_string(),
                    tasks: vec![
                        ProgressTask {
                            id: "t1".to_string(),
                            state: TaskState::Doing,
                            note: "migration".to_string(),
                        },
                        ProgressTask {
                            id: "t2".to_string(),
                            state: TaskState::Blocked,
                            note: String::new(),
                        },
                    ],
                    off_plan: vec!["renamed a helper".to_string()],
                },
                seq: 2,
                age_ms: 90 * 1000,
            }),
        }
    }

    fn plan_version(
        version: i64,
        kind: crate::plan::VersionKind,
        body: &str,
    ) -> crate::plan::PlanVersion {
        crate::plan::PlanVersion {
            version,
            body: body.to_string(),
            summary: format!("summary {version}"),
            kind,
            created_at_ms: 1_000,
            approved_at_ms: (kind == crate::plan::VersionKind::OperatorApproved).then_some(4_000),
        }
    }

    fn with_plan(
        pending: Option<crate::plan::PlanVersion>,
        in_force: Option<crate::plan::PlanVersion>,
    ) -> BrigadeReading {
        BrigadeReading {
            decision: None,
            progress: None,
            plan: Some(Box::new(PlanReading {
                view: crate::plan::PlanView {
                    plan: crate::plan::Plan {
                        number: 3,
                        title: "Split the store".to_string(),
                        request: "split it".to_string(),
                        state: if in_force.is_some() {
                            PlanState::Approved
                        } else {
                            PlanState::Drafting
                        },
                        result: None,
                        closed_by: None,
                        opened_at_ms: 500,
                        closed_at_ms: None,
                    },
                    in_force,
                    pending,
                },
                read_at_ms: 7_201_000,
            })),
        }
    }

    #[test]
    fn the_plan_comes_after_progress_and_says_none_is_open_when_there_is_none() {
        let lines = texts(&reading_lines(&loaded(sample()), now(), 60));
        let progress = lines.iter().position(|l| l == "Progress").unwrap();
        let plan = lines.iter().position(|l| l == "Plan").unwrap();
        assert!(progress < plan, "{lines:?}");
        assert_eq!(lines.last().map(String::as_str), Some("No Plan is open."));
    }

    #[test]
    fn a_draft_is_shown_before_the_version_in_force_with_both_bodies_in_full() {
        use crate::plan::VersionKind::{Draft, OperatorApproved};
        let reading = with_plan(
            Some(plan_version(2, Draft, "the new body\nsecond line")),
            Some(plan_version(1, OperatorApproved, "the old body")),
        );
        let state = loaded(reading);
        assert_eq!(state.approvable(), Some((3, 2)));
        let lines = texts(&reading_lines(&state, now(), 60));
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle}: {lines:?}"))
        };
        assert!(at("Plan 3") < at("Draft, version 2"));
        assert!(at("Draft, version 2") < at("the new body"));
        assert!(at("the new body") < at("second line"));
        assert!(at("second line") < at("In force, version 1"));
        assert!(at("In force, version 1") < at("the old body"));
        assert!(lines[at("Draft, version 2")].contains("awaits your approval"));
        // Ages are the shell's read time minus the stored time: two hours for the
        // draft, an hour and fifty-nine minutes for the approval.
        assert!(
            lines[at("Draft, version 2") + 1].contains("written 2h ago"),
            "{lines:?}"
        );
        assert!(
            lines[at("In force, version 1") + 1].contains("approved 1h ago"),
            "{lines:?}"
        );
    }

    #[test]
    fn with_nothing_drafted_there_is_nothing_to_approve() {
        use crate::plan::VersionKind::OperatorApproved;
        let state = loaded(with_plan(
            None,
            Some(plan_version(1, OperatorApproved, "in force")),
        ));
        assert_eq!(state.approvable(), None);
        assert_eq!(loaded(sample()).approvable(), None);
        let lines = texts(&reading_lines(&state, now(), 60));
        assert!(!lines.iter().any(|l| l.contains("awaits your approval")));
    }

    #[test]
    fn nothing_is_approvable_until_the_body_has_loaded() {
        let state = ReadingState::new(1, "alpha".to_string(), ReadingTrigger::default());
        assert_eq!(state.approvable(), None);
    }

    #[test]
    fn a_plan_body_is_wrapped_by_display_width() {
        use crate::plan::VersionKind::Draft;
        let body = "あいうえおかきくけこ";
        let state = loaded(with_plan(Some(plan_version(1, Draft, body)), None));
        let lines = texts(&reading_lines(&state, now(), 9));
        assert!(lines.contains(&"あいうえ".to_string()), "{lines:?}");
        assert!(lines.contains(&"おかきく".to_string()), "{lines:?}");
        // The Plan section is wrapped throughout; the sections above it have short
        // fixed texts of their own.
        let plan = lines.iter().position(|l| l == "Plan").unwrap();
        assert!(lines[plan..].iter().all(|l| l.width() <= 9), "{lines:?}");
    }

    #[test]
    fn wrapping_counts_display_width_so_a_japanese_line_breaks_at_half_the_characters() {
        // Ten two-column characters in nine columns: four to a line, never five.
        let lines = wrap_to_width("あいうえおかきくけこ", 9);
        assert_eq!(lines, ["あいうえ", "おかきく", "けこ"]);
        assert!(lines.iter().all(|line| line.width() <= 9));
    }

    #[test]
    fn wrapping_breaks_at_a_space_when_one_fits_and_keeps_line_breaks() {
        assert_eq!(
            wrap_to_width("one two three\nfour", 9),
            ["one two", "three", "four"]
        );
        // A word longer than the room is broken inside.
        assert_eq!(wrap_to_width("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        // Nothing fits one column but a character still gets its line.
        assert_eq!(wrap_to_width("あい", 1), ["あ", "い"]);
        assert_eq!(wrap_to_width("", 5), [""]);
    }

    #[test]
    fn a_break_at_a_space_never_leaves_a_line_wider_than_the_room() {
        // The kept tail plus the character that did not fit was appended unchecked:
        // " あいう" in five columns came out as ["", "あいう"], six columns wide.
        assert_eq!(wrap_to_width(" あいう", 5), [" あい", "う"]);
        for text in [
            " あいう",
            "  あいうえお かきくけこ",
            "ab あいうえお",
            "a あ い う",
        ] {
            for width in 2..14 {
                for line in wrap_to_width(text, width) {
                    assert!(
                        line.width() <= width,
                        "{text:?} at {width}: {line:?} is {} wide",
                        line.width()
                    );
                }
            }
        }
        // An indent is not a place to break: nothing is left before it.
        assert_eq!(wrap_to_width("  abcdef", 4), ["  ab", "cdef"]);
    }

    #[test]
    fn a_lead_as_wide_as_the_window_gets_its_own_line_and_the_note_stays_readable() {
        // A valid sixteen-character Japanese id is thirty-two columns; with the state
        // word and the gaps the lead is forty-four, wider than a forty-column text. The
        // note used to start at column forty-four and be clipped away.
        let id: String = "あ".repeat(16);
        let mut state = ReadingState::new(1, "alpha".to_string(), ReadingTrigger::default());
        state.set_loaded(
            BrigadeReading {
                plan: None,
                decision: None,
                progress: Some(ProgressReading {
                    progress: Progress {
                        stage: "s".to_string(),
                        tasks: vec![ProgressTask {
                            id: id.clone(),
                            state: TaskState::Doing,
                            note: "the note that has to be read in full".to_string(),
                        }],
                        off_plan: Vec::new(),
                    },
                    seq: 1,
                    age_ms: 0,
                }),
            },
            now(),
        );
        let full = format!("doing{id}thenotethathastobereadinfull");
        for width in [40, 20, 8, 3, 2, 1] {
            let lines = reading_lines(&state, now(), width);
            let task: Vec<String> = lines
                .iter()
                .map(ReadingLine::plain)
                .skip_while(|text| text.trim() != "Tasks")
                .skip(1)
                .take_while(|text| !text.is_empty())
                .collect();
            // Nothing of the task is lost to the edge, the id included: the lead is
            // wrapped like the note, and the note's indent gives way to a wide
            // character.
            assert_eq!(
                task.join("").replace(' ', ""),
                full,
                "width {width}: {task:?}"
            );
            for text in &task {
                assert!(text.width() <= width.max(2), "width {width}: {text:?}");
            }
        }
        // The ordinary case still hangs: the note starts beside the id.
        let wide = reading_lines(&state, now(), 100);
        assert!(
            wide.iter().any(|line| {
                let text = line.plain();
                text.contains(&id) && text.ends_with("the note that has to be read in full")
            }),
            "{wide:?}"
        );
    }

    #[test]
    fn a_stacked_note_gives_up_its_indent_before_a_wide_character_is_clipped() {
        let mut lines = Vec::new();
        let lead = vec![("  doing  t  ".to_string(), Tone::Plain)];
        for width in [2usize, 3, 4] {
            lines.clear();
            hanging(&mut lines, lead.clone(), "あい", Tone::Plain, width);
            let note: Vec<String> = lines
                .iter()
                .map(ReadingLine::plain)
                .filter(|text| text.contains('あ') || text.contains('い'))
                .collect();
            assert_eq!(note.join("").trim().replace(' ', ""), "あい", "{width}");
            for text in &note {
                assert!(text.width() <= width, "width {width}: {text:?}");
            }
        }
    }

    #[test]
    fn a_wrapped_lead_keeps_the_tone_of_each_piece() {
        let pieces = wrap_spans(
            vec![
                ("ab".to_string(), Tone::Heading),
                ("cde  ".to_string(), Tone::Dim),
            ],
            3,
        );
        let flat: Vec<Vec<(&str, Tone)>> = pieces
            .iter()
            .map(|line| line.iter().map(|(t, tone)| (t.as_str(), *tone)).collect())
            .collect();
        assert_eq!(
            flat,
            [
                vec![("ab", Tone::Heading), ("c", Tone::Dim)],
                vec![("de ", Tone::Dim)],
            ]
        );
    }

    #[test]
    fn the_screen_says_so_when_there_is_nothing_to_read() {
        let state = loaded(BrigadeReading {
            plan: None,
            decision: None,
            progress: None,
        });
        let lines = texts(&reading_lines(&state, now(), 40));
        assert!(lines.contains(&"No decision is open.".to_string()));
        assert!(lines.contains(&"None written yet.".to_string()));
    }

    #[test]
    fn a_decision_without_a_question_points_at_the_directors_pane() {
        let state = loaded(BrigadeReading {
            plan: None,
            decision: Some(DecisionReading {
                seq: 1,
                question: None,
                age_ms: None,
            }),
            progress: None,
        });
        let joined = texts(&reading_lines(&state, now(), 80)).join("\n");
        assert!(joined.contains("in the Director's own pane"));
        assert!(!joined.contains("asked "), "no time, no age line");
    }

    #[test]
    fn the_question_progress_and_ages_are_laid_out_and_the_ages_advance_while_open() {
        let state = loaded(sample());
        let read_at = match state.body() {
            ReadingBody::Loaded { read_at, .. } => *read_at,
            _ => unreachable!(),
        };
        let at_read = texts(&reading_lines(&state, read_at, 60));
        assert!(at_read.contains(&"asked 5m ago".to_string()));
        assert!(at_read.contains(&"Ship it now?".to_string()));
        assert!(at_read.contains(&"Or wait for the review.".to_string()));
        assert!(at_read.contains(&"written 1m ago".to_string()));
        assert!(at_read.contains(&"Stage  implementing the store".to_string()));
        assert!(at_read.contains(&"  doing   t1  migration".to_string()));
        assert!(at_read.contains(&"  blocked t2  ".to_string()));
        assert!(at_read.contains(&"  \u{2022} renamed a helper".to_string()));
        let later = texts(&reading_lines(
            &state,
            read_at + Duration::from_secs(10 * 60),
            60,
        ));
        assert!(later.contains(&"asked 15m ago".to_string()));
    }

    #[test]
    fn a_long_note_hangs_under_its_text_not_under_its_state_word() {
        let mut reading = sample();
        reading.progress.as_mut().unwrap().progress.tasks[0].note =
            "alpha beta gamma delta epsilon".to_string();
        let state = loaded(reading);
        let lines = texts(&reading_lines(&state, now(), 26));
        let first = lines
            .iter()
            .position(|l| l.starts_with("  doing   t1  alpha"))
            .expect("the task's first line");
        // "  doing   t1  " is 14 columns, so the continuation is indented by 14.
        assert!(lines[first + 1].starts_with(&" ".repeat(14)));
        assert!(lines[first + 1].trim_start().starts_with("gamma"));
    }

    #[test]
    fn the_scroll_limit_is_the_lines_that_do_not_fit() {
        let state = loaded(sample());
        let tall = reading_max_scroll(&state, now(), (120, 60));
        assert_eq!(tall, 0, "everything fits");
        let short = reading_max_scroll(&state, now(), (120, 12));
        let total =
            reading_lines(&state, now(), reading_text_size(Rect::new(0, 0, 120, 12)).0).len();
        assert_eq!(short, total - (12 - 4 - 2));
    }

    #[test]
    fn the_area_keeps_a_margin_and_a_cap_and_never_collapses_to_nothing() {
        let wide = reading_area(Rect::new(0, 0, 200, 50));
        assert_eq!((wide.width, wide.height), (MAX_WIDTH, 46));
        assert_eq!(wide.x, 50);
        let narrow = reading_area(Rect::new(0, 0, 3, 3));
        assert!(narrow.width >= 1 && narrow.height >= 1);
        let (w, h) = reading_text_size(Rect::new(0, 0, 3, 3));
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn a_failed_read_is_said_with_its_reason() {
        let mut state = ReadingState::new(1, "a".to_string(), ReadingTrigger::default());
        state.set_unreadable("database is locked".to_string());
        let joined = texts(&reading_lines(&state, now(), 80)).join("\n");
        assert!(joined.contains("Could not read this brigade: database is locked"));
        assert!(!state.decision_open());
    }

    #[test]
    fn a_refresh_keeps_the_scroll_position() {
        let mut state = loaded(sample());
        state.set_scroll(4);
        state.set_loaded(sample(), now());
        assert_eq!(state.scroll(), 4);
        assert!(state.decision_open());
    }
}
