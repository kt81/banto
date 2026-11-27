//! A brigade's Plan: what the operator and a Planner worked out, the rules for
//! writing and reading it, and the words banto says about it. Pure.
//!
//! A Plan is opened together with its Planner and closed with it (one open Plan
//! per brigade, one Planner per brigade, one lifetime). Its text is Markdown in
//! the operator's language; banto stores and draws it and never interprets it.
//! Versions only go forward. A Planner's `write_plan` adds a **draft**; the
//! operator's key on the reading screen turns the draft they were shown into the
//! version **in force**; after that the Planner may release a draft itself, but
//! only for changes the operator left to it. So the version in force is always
//! the newest version that is not a draft, and a **pending** draft is the newest
//! version when that one is a draft: there is never more than one of either, and
//! the store needs no flag to say which.
//!
//! Who may read a draft's body is decided here ([`read_plan_text`]): Workers must
//! not implement an unapproved draft, and the Director's context should not carry
//! two bodies.
//!
//! Limits count characters (Unicode scalar values), as in `progress`: a bound on
//! what is stored, not on how wide it draws.

use serde::{Deserialize, Serialize};

use crate::config::GoinkyoConsult;
use crate::display_text::{display_lines, display_name};
use crate::model::{BrigadeId, BrigadeRole};
use crate::progress::{Progress, ProgressTask, TaskState};

/// The longest Plan body, in characters, line breaks included. A Plan is a spec,
/// a task breakdown and completion conditions; a bigger one is a document that
/// belongs in the repository and is linked from here.
pub const PLAN_BODY_MAX_CHARS: usize = 30_000;
/// The longest one-line summary of a version, in characters.
pub const PLAN_SUMMARY_MAX_CHARS: usize = 200;
/// The longest `request`, and the longest `known`, in characters.
pub const PLAN_REQUEST_MAX_CHARS: usize = 20_000;
/// The most versions one Plan holds. The Planner rewrites the whole body each
/// time, so this bounds the table and not any one reader's context.
pub const PLAN_VERSIONS_MAX: i64 = 100;

/// Where a Plan stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanState {
    /// No version has been approved yet.
    Drafting,
    /// The operator has approved a version; a newer draft may be pending.
    Approved,
    Closed,
}

impl PlanState {
    pub fn as_token(self) -> &'static str {
        match self {
            PlanState::Drafting => "drafting",
            PlanState::Approved => "approved",
            PlanState::Closed => "closed",
        }
    }

    pub fn from_token(word: &str) -> Option<PlanState> {
        [PlanState::Drafting, PlanState::Approved, PlanState::Closed]
            .into_iter()
            .find(|state| state.as_token() == word)
    }
}

/// How a closed Plan ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanResult {
    Completed,
    Abandoned,
}

impl PlanResult {
    pub fn as_token(self) -> &'static str {
        match self {
            PlanResult::Completed => "completed",
            PlanResult::Abandoned => "abandoned",
        }
    }

    pub fn from_token(word: &str) -> Option<PlanResult> {
        [PlanResult::Completed, PlanResult::Abandoned]
            .into_iter()
            .find(|result| result.as_token() == word)
    }
}

/// Who closed a Plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanCloser {
    /// The Director's `close_planner`.
    Director,
    /// The operator: prefix-`x` on the Planner's tile, or closing or disbanding
    /// the brigade.
    Operator,
}

impl PlanCloser {
    pub fn as_token(self) -> &'static str {
        match self {
            PlanCloser::Director => "director",
            PlanCloser::Operator => "operator",
        }
    }

    pub fn from_token(word: &str) -> Option<PlanCloser> {
        [PlanCloser::Director, PlanCloser::Operator]
            .into_iter()
            .find(|closer| closer.as_token() == word)
    }
}

/// What a version is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionKind {
    /// Written by the Planner and not yet approved or released.
    Draft,
    /// A draft the operator approved with a key on the reading screen.
    OperatorApproved,
    /// A draft the Planner released itself, on an already approved Plan.
    PlannerReleased,
}

impl VersionKind {
    pub fn as_token(self) -> &'static str {
        match self {
            VersionKind::Draft => "draft",
            VersionKind::OperatorApproved => "operator_approved",
            VersionKind::PlannerReleased => "planner_released",
        }
    }

    pub fn from_token(word: &str) -> Option<VersionKind> {
        [
            VersionKind::Draft,
            VersionKind::OperatorApproved,
            VersionKind::PlannerReleased,
        ]
        .into_iter()
        .find(|kind| kind.as_token() == word)
    }

    /// Whether this version is in force once it is the newest of its kind: any
    /// version that is not a draft.
    pub fn in_force(self) -> bool {
        self != VersionKind::Draft
    }
}

/// One version of a Plan's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanVersion {
    pub version: i64,
    pub body: String,
    /// One line, for the Director, the message that announces the version and
    /// the screen's title for it.
    pub summary: String,
    pub kind: VersionKind,
    pub created_at_ms: i64,
    /// When the operator approved it; `None` for a draft and for a version the
    /// Planner released.
    pub approved_at_ms: Option<i64>,
}

/// A brigade's Plan as the store holds it, without its versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The Plan's number within its brigade, from 1: the export's file name.
    pub number: i64,
    pub title: String,
    /// The operator's request as the Director passed it, reduced for display.
    pub request: String,
    pub state: PlanState,
    pub result: Option<PlanResult>,
    pub closed_by: Option<PlanCloser>,
    pub opened_at_ms: i64,
    pub closed_at_ms: Option<i64>,
}

/// A Plan with the two versions that matter: the one in force and the pending
/// draft. Each is `None` when there is none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanView {
    pub plan: Plan,
    pub in_force: Option<PlanVersion>,
    /// Newer than `in_force`, and a draft.
    pub pending: Option<PlanVersion>,
}

/// What the tick knows about a brigade's open Plan without reading a body: the
/// write count says whether the screen's text is stale, the state and the shown
/// version are the strip's and the hint line's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanGlance {
    pub number: i64,
    /// The newest version's number, `0` before the first draft.
    pub latest_version: i64,
    pub state: PlanState,
    /// Whether the newest version is a draft: something for the operator to
    /// approve.
    pub pending: bool,
}

/// What the reading screen draws of the brigade's open Plan: the store's view and
/// the moment, in unix milliseconds by the clock of the shell that read it, that
/// ages are counted from. The core reads no clock, so an age is the difference of
/// two stored times plus the time the screen has been open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReading {
    pub view: PlanView,
    pub read_at_ms: i64,
}

impl PlanReading {
    /// The Plan's number and the version the approval key would approve: the
    /// pending draft, which is what the screen shows first. `None` when there is
    /// nothing to approve.
    pub fn approvable(&self) -> Option<(i64, i64)> {
        self.view
            .pending
            .as_ref()
            .map(|draft| (self.view.plan.number, draft.version))
    }
}

/// What the operator's approval did, as the status line tells it. The export is
/// written after the approval is stored, and a failure of it never undoes the
/// approval: the database holds the Plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanApprovalReport {
    Approved {
        number: i64,
        version: i64,
        /// The file written, or why none was.
        export: Result<String, String>,
    },
    /// A newer version than the one on screen exists; nothing was approved.
    Stale { latest: i64 },
    /// The newest version is not a draft, or there is none.
    NothingToApprove,
    /// The Plan closed, or another opened, since the screen read it.
    NoOpenPlan,
}

impl PlanApprovalReport {
    pub fn status_line(&self) -> String {
        match self {
            PlanApprovalReport::Approved {
                number,
                version,
                export: Ok(path),
            } => format!("Plan {number} version {version} approved \u{2014} exported to {path}"),
            PlanApprovalReport::Approved {
                number,
                version,
                export: Err(error),
            } => format!("Plan {number} version {version} approved; its export failed: {error}"),
            PlanApprovalReport::Stale { latest } => format!(
                "Not approved: the Planner has written version {latest} since this was read. \
                 Read the new draft, then press A"
            ),
            PlanApprovalReport::NothingToApprove => {
                "Nothing to approve: the newest version is not a draft".to_string()
            }
            PlanApprovalReport::NoOpenPlan => "That Plan is no longer open".to_string(),
        }
    }
}

/// Why `write_plan` or `open_planner` was refused, worded for the agent that
/// made the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanRejection {
    /// `what` has nothing visible once reduced.
    NothingVisible { what: &'static str },
    TooLong {
        what: &'static str,
        limit: usize,
        chars: usize,
    },
}

impl std::fmt::Display for PlanRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanRejection::NothingVisible { what } => write!(
                f,
                "`{what}` has nothing visible in it (control and invisible characters are \
                 dropped)."
            ),
            PlanRejection::TooLong { what, limit, chars } => write!(
                f,
                "`{what}` is {chars} characters; the limit is {limit}. Nothing was written. \
                 Shorten it: a Plan is a spec, tasks and completion conditions, and what \
                 does not fit belongs in a document the Plan points to."
            ),
        }
    }
}

fn check_text(what: &'static str, input: &str, limit: usize) -> Result<String, PlanRejection> {
    let text = display_lines(input);
    if text.is_empty() {
        return Err(PlanRejection::NothingVisible { what });
    }
    let chars = text.chars().count();
    if chars > limit {
        return Err(PlanRejection::TooLong { what, limit, chars });
    }
    Ok(text)
}

/// A `write_plan` call checked and reduced: the body keeps its lines, the summary
/// is folded to one. `Ok` is `(body, summary)` as stored. Nothing is cut: a text
/// over the limit is refused, so the Planner sees that it was.
pub fn check_write(body: &str, summary: &str) -> Result<(String, String), PlanRejection> {
    let body = check_text("body", body, PLAN_BODY_MAX_CHARS)?;
    let summary = display_name(summary).ok_or(PlanRejection::NothingVisible { what: "summary" })?;
    let chars = summary.chars().count();
    if chars > PLAN_SUMMARY_MAX_CHARS {
        return Err(PlanRejection::TooLong {
            what: "summary",
            limit: PLAN_SUMMARY_MAX_CHARS,
            chars,
        });
    }
    Ok((body, summary))
}

/// `open_planner`'s `request` (required) reduced for storage and display.
pub fn check_request(input: &str) -> Result<String, PlanRejection> {
    check_text("request", input, PLAN_REQUEST_MAX_CHARS)
}

/// `open_planner`'s `known` (optional) reduced; `Ok(None)` when nothing visible is
/// left, which is the same as passing none.
pub fn check_known(input: &str) -> Result<Option<String>, PlanRejection> {
    match check_text("known", input, PLAN_REQUEST_MAX_CHARS) {
        Ok(text) => Ok(Some(text)),
        Err(PlanRejection::NothingVisible { .. }) => Ok(None),
        Err(other) => Err(other),
    }
}

/// Why `completed` is refused. Abandoning is never refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionRefusal {
    /// The operator never approved a version (the Plan is still drafting).
    NeverApproved,
    /// The Director has written no task into Progress at all.
    NoTasks,
    /// A task is still `todo`, `doing` or `blocked`.
    OpenTasks(Vec<String>),
}

impl std::fmt::Display for CompletionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompletionRefusal::NeverApproved => write!(
                f,
                "this Plan was never approved by the operator, so it cannot be completed. \
                 Close it with result `abandoned` if the work is not going ahead."
            ),
            CompletionRefusal::NoTasks => write!(
                f,
                "Progress has no task at all, so there is nothing to call done. Copy the \
                 Plan's tasks into Progress with update_progress and mark them, or close with \
                 result `abandoned`."
            ),
            CompletionRefusal::OpenTasks(ids) => write!(
                f,
                "Progress still has open tasks ({}). A task is closed as `done` or `dropped`; \
                 update Progress first, or close with result `abandoned`.",
                ids.join(", ")
            ),
        }
    }
}

fn is_open(task: &ProgressTask) -> bool {
    matches!(
        task.state,
        TaskState::Todo | TaskState::Doing | TaskState::Blocked
    )
}

/// Whether a Plan in `state` with this Progress may close as `completed`: it was
/// approved, Progress has tasks, and none of them is `todo`, `doing` or
/// `blocked`. `None` is "yes".
pub fn completion_refusal(
    state: PlanState,
    progress: Option<&Progress>,
) -> Option<CompletionRefusal> {
    if state != PlanState::Approved {
        return Some(CompletionRefusal::NeverApproved);
    }
    let tasks = progress.map(|p| p.tasks.as_slice()).unwrap_or_default();
    if tasks.is_empty() {
        return Some(CompletionRefusal::NoTasks);
    }
    let open: Vec<String> = tasks
        .iter()
        .filter(|task| is_open(task))
        .map(|task| task.id.clone())
        .collect();
    (!open.is_empty()).then_some(CompletionRefusal::OpenTasks(open))
}

/// The result the operator's prefix-`x` on the Planner's tile closes with:
/// `completed` when its conditions hold, otherwise `abandoned`.
pub fn operator_close_result(state: PlanState, progress: Option<&Progress>) -> PlanResult {
    if completion_refusal(state, progress).is_none() {
        PlanResult::Completed
    } else {
        PlanResult::Abandoned
    }
}

/// The text of `read_plan` for a caller of `role`: the request, the version in
/// force with how it came to be in force, and what is pending.
///
/// Only the Planner is given a pending draft's body. Anyone else is told that one
/// awaits the operator, or, before any approval, that the Plan is drafting with
/// no approved version: what is not approved is not for Workers to act on.
pub fn read_plan_text(role: BrigadeRole, view: Option<&PlanView>) -> String {
    let Some(view) = view else {
        return "This brigade has no open Plan.".to_string();
    };
    let plan = &view.plan;
    let mut out = format!(
        "Plan {} \u{2014} {}\nState: {}\n\nRequest (the operator's words):\n{}\n",
        plan.number,
        plan.title,
        plan.state.as_token(),
        plan.request
    );
    match &view.in_force {
        Some(version) => {
            let by = match version.kind {
                VersionKind::PlannerReleased => {
                    "released by the Planner within what the operator \
                                                 approved"
                }
                _ => "approved by the operator",
            };
            out.push_str(&format!(
                "\nVersion in force: {} ({by}) \u{2014} {}\n\n{}\n",
                version.version, version.summary, version.body
            ));
        }
        None => out.push_str("\nNo version has been approved yet.\n"),
    }
    match (&view.pending, role) {
        (Some(draft), BrigadeRole::Planner) => out.push_str(&format!(
            "\nYour latest draft, version {} (not approved; it awaits the operator) \u{2014} {}\n\n{}\n",
            draft.version, draft.summary, draft.body
        )),
        (Some(_), _) if view.in_force.is_none() => out.push_str(
            "\nDrafting, no approved version. A draft awaits the operator's approval, and \
             nothing is settled: do not start work from this Plan.\n",
        ),
        (Some(_), _) => out.push_str(
            "\nA newer draft awaits the operator's approval. Do not act on it; the version in \
             force above is what stands.\n",
        ),
        (None, _) if view.in_force.is_none() => out.push_str(
            "\nDrafting, no approved version. Nothing is settled; do not start work from this \
             Plan.\n",
        ),
        (None, _) => {}
    }
    out
}

/// [`read_plan_text`] as the parts one `read_plan` call hands over, each at most
/// `budget` bytes of UTF-8 with its own header and footer. A view that fits is a
/// single part and its text is `read_plan_text`'s, byte for byte.
///
/// The view is cut because the reader's product cuts a long tool output in the
/// middle without an error; unlike a mailbox, a Plan can be asked for again, so
/// the cut is made here, at line boundaries, and the reader asks for the rest.
pub fn read_plan_parts(role: BrigadeRole, view: Option<&PlanView>, budget: usize) -> Vec<String> {
    let text = read_plan_text(role, view);
    let Some(view) = view else {
        return vec![text];
    };
    let number = view.plan.number;
    let in_force = view.in_force.as_ref().map(|version| version.version);
    let draft = view
        .pending
        .as_ref()
        .filter(|_| role == BrigadeRole::Planner)
        .map(|version| version.version);
    split_into_parts(
        &text,
        budget,
        |part, of| part_header(number, in_force, draft, part, of),
        part_footer,
    )
}

/// What a part of a view begins with when the view has more than one: the Plan, the
/// versions its text shows (a draft only where the reader is shown one), and where
/// the part stands. The versions are there because a Plan can change between two
/// calls, and a reader holding parts of two versions must know to start over.
pub fn part_header(
    number: i64,
    in_force: Option<i64>,
    draft: Option<i64>,
    part: usize,
    of: usize,
) -> String {
    let in_force = in_force.map_or_else(|| "none".to_string(), |version| version.to_string());
    let draft = draft.map_or_else(String::new, |version| format!(", draft {version}"));
    format!("[banto] Plan {number}, version in force {in_force}{draft}, part {part} of {of}\n\n")
}

/// What every part but the last ends with.
pub fn part_footer(part: usize) -> String {
    format!(
        "\n[banto] More of this Plan follows: call read_plan with part = {}.\n",
        part + 1
    )
}

/// Split `text` into parts of at most `budget` bytes, header and footer counted,
/// cut at line boundaries (a line longer than a part is cut at a character
/// boundary). Text that fits is returned whole with no header and no footer. Taking
/// the parts apart again, `header(i, n)` + piece + `footer(i)` for every part but
/// the last, which has no footer, gives the pieces back in order, and joined they
/// are `text`.
///
/// The frame's width depends on how many digits the part numbers have, and the
/// count of parts on the width of the frame, so the room is reserved for the
/// widest numbers of a given digit count and that count is raised until the parts
/// that result need no more digits than it was reserved for. A frame never
/// outgrows its reservation, so no part exceeds the budget, provided the budget
/// leaves room for the frame of the widest part numbers it needs and four bytes
/// (the longest character) of text beside it. A budget below that cannot be kept:
/// the pieces are then four bytes or more and the parts exceed it, and a debug
/// build stops there. The caller passes a budget measured in kilobytes against a
/// frame of about a hundred bytes.
pub fn split_into_parts(
    text: &str,
    budget: usize,
    header: impl Fn(usize, usize) -> String,
    footer: impl Fn(usize) -> String,
) -> Vec<String> {
    if text.len() <= budget {
        return vec![text.to_string()];
    }
    for digits in 1..=usize::MAX.ilog10() as usize {
        let widest = 10usize.pow(digits as u32) - 1;
        let frame = header(widest, widest).len() + footer(widest).len();
        // Room for a character, whatever the frame takes: a budget that small is
        // not a real one, and the cut below needs at least four bytes to place.
        let room = budget.saturating_sub(frame).max(4);
        let pieces = split_at_lines(text, room);
        if pieces.len().to_string().len() > digits {
            continue;
        }
        debug_assert!(
            budget >= frame + 4,
            "a budget of {budget} bytes cannot hold a frame of {frame} and a character"
        );
        let of = pieces.len();
        return pieces
            .into_iter()
            .enumerate()
            .map(|(index, piece)| {
                let part = index + 1;
                let footer = if part < of {
                    footer(part)
                } else {
                    String::new()
                };
                format!("{}{piece}{footer}", header(part, of))
            })
            .collect();
    }
    vec![text.to_string()]
}

/// `text` in consecutive pieces of at most `room` bytes, each ending at the end of
/// a line when it can. `room` is at least four, the longest character.
fn split_at_lines(text: &str, room: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut end = 0;
    for line in text.split_inclusive('\n') {
        if end - start + line.len() <= room {
            end += line.len();
            continue;
        }
        if end > start {
            pieces.push(&text[start..end]);
            start = end;
        }
        // A line that is more than a piece is cut where a character ends; what is
        // left of it, short enough, goes on being filled by the lines after it.
        let mut rest = line;
        while rest.len() > room {
            let mut at = room;
            while !rest.is_char_boundary(at) {
                at -= 1;
            }
            pieces.push(&text[start..start + at]);
            start += at;
            rest = &rest[at..];
        }
        end = start + rest.len();
    }
    if end > start {
        pieces.push(&text[start..end]);
    }
    pieces
}

/// The file a Planner reads on its first turn: the operator's request in their own
/// words, and what the Director already knows, kept apart so the Planner can tell
/// which is which. The first turn cannot call banto's tools (they are not
/// connected yet), so this file is the whole of what it is handed.
pub fn render_request_file(title: &str, request: &str, known: Option<&str>) -> String {
    let mut out = format!(
        "Plan request: {title}\n\nWhat the operator asked for, in their words (verbatim):\n{request}\n"
    );
    if let Some(known) = known {
        out.push_str(&format!(
            "\nWhat the Director already knows (the Director's words, not the operator's):\n{known}\n"
        ));
    }
    out
}

/// What a closed Plan's export says about how it ended.
fn ending(plan: &Plan) -> String {
    match (plan.result, plan.closed_by) {
        (Some(result), Some(by)) => format!("{} by the {}", result.as_token(), by.as_token()),
        (Some(result), None) => result.as_token().to_string(),
        _ => "open".to_string(),
    }
}

/// The file banto keeps of a Plan: its title and state, the text, and on close the
/// final Progress. The text is the version in force; when none was ever approved
/// it is the newest draft, and says so. Pure: the shell writes it, and a failed
/// write never fails the approval or close it came from, since the database is
/// the source of truth.
pub fn render_export(
    brigade: BrigadeId,
    view: &PlanView,
    final_progress: Option<&Progress>,
) -> String {
    let plan = &view.plan;
    let mut out = format!("# Plan {} \u{2014} {}\n\n", plan.number, plan.title);
    out.push_str(&format!("Brigade: {brigade}\n"));
    match plan.state {
        PlanState::Closed => out.push_str(&format!("State: closed ({})\n", ending(plan))),
        state => out.push_str(&format!("State: {}\n", state.as_token())),
    }
    out.push_str(&format!("\n## Request\n\n{}\n", plan.request));
    match (&view.in_force, &view.pending) {
        (Some(version), pending) => {
            let by = match version.kind {
                VersionKind::PlannerReleased => "released by the Planner",
                _ => "approved by the operator",
            };
            out.push_str(&format!(
                "\n## Plan \u{2014} version {} ({by})\n\n{}\n",
                version.version, version.body
            ));
            if let Some(draft) = pending {
                out.push_str(&format!(
                    "\n(A newer draft, version {}, was never approved and is not included.)\n",
                    draft.version
                ));
            }
        }
        (None, Some(draft)) => out.push_str(&format!(
            "\n## Plan \u{2014} DRAFT, version {} (never approved)\n\n{}\n",
            draft.version, draft.body
        )),
        (None, None) => out.push_str("\n## Plan\n\n(No version was ever written.)\n"),
    }
    if let Some(progress) = final_progress {
        out.push_str(&format!("\n## Progress at the end\n\n{}\n", progress.stage));
        for task in &progress.tasks {
            if task.note.is_empty() {
                out.push_str(&format!("- {} [{}]\n", task.id, task.state.as_token()));
            } else {
                out.push_str(&format!(
                    "- {} [{}] {}\n",
                    task.id,
                    task.state.as_token(),
                    task.note
                ));
            }
        }
        if !progress.off_plan.is_empty() {
            out.push_str("\nOutside the Plan:\n");
            for item in &progress.off_plan {
                out.push_str(&format!("- {item}\n"));
            }
        }
    }
    out
}

/// What the Director is told to do about a review of an approved Plan, by the
/// operator's `goinkyo_consult`. It is said in the approval message and not in the
/// briefing: the briefing has no room for it (a Codex Director's is near the size
/// past which Codex cannot match it again), and the message arrives when the Director
/// acts on it. `None` is an empty setting, which gives the Director no consultation
/// instruction at all, so it reads as `off`.
pub fn review_step(level: Option<GoinkyoConsult>) -> &'static str {
    match level {
        None | Some(GoinkyoConsult::Off) | Some(GoinkyoConsult::Disputes) => {
            "Send it to the Goinkyo for review only if the operator asks. If you do, build \
             nothing until the consultation is answered; its findings go to the Planner with \
             send_to_peer and to Progress, and any that touch scope or the completion \
             conditions also go to the operator with request_decision."
        }
        Some(GoinkyoConsult::Design) => {
            "If it contains a lasting change, as your consultation paragraph defines one, send \
             it to the Goinkyo with kind \"design\" before the first errand that builds it; \
             otherwise no review is needed. While a review is out, build nothing; its findings \
             go to the Planner with send_to_peer and to Progress, and any that touch scope or \
             the completion conditions also go to the operator with request_decision."
        }
        Some(GoinkyoConsult::Decisions) => {
            "Send it to the Goinkyo with kind \"design\" before the first errand that builds \
             it. While the review is out, build nothing; its findings go to the Planner with \
             send_to_peer and to Progress, and any that touch scope or the completion \
             conditions also go to the operator with request_decision."
        }
    }
}

/// The message banto writes to the Director in the transaction that records the
/// operator's approval. banto's own words, from [`crate::model::BANTO_SENDER`]:
/// nothing about it came from another AI.
pub fn approval_notice(number: i64, version: i64, level: Option<GoinkyoConsult>) -> String {
    format!(
        "The operator approved Plan {number}, version {version}, with the key on banto's \
         reading screen. Call read_plan to see what is now in force, and copy its tasks into \
         Progress with update_progress. Review: {}",
        review_step(level)
    )
}

/// The message banto writes to the Director in the transaction in which the
/// operator closes the Plan from the Planner's tile. Without it the Director would
/// go on believing in a Planner that is gone.
pub fn operator_close_notice(number: i64, result: PlanResult) -> String {
    format!(
        "The operator closed Plan {number} ({}) from the Planner's tile. The Planner has \
         ended and the brigade has no open Plan.",
        result.as_token()
    )
}

/// Who closed a Plan from the UI's side: the two ways the UI process closes one.
/// (The Director's `close_planner` runs in its own `_mcp` process and no event
/// reports it.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanCloseCause {
    /// Prefix-`x` on the Planner's tile, confirmed.
    Operator,
    /// The brigade was disbanded.
    Disband,
}

/// What closing a Plan did, for the status line: which Plan, how it ended, and how
/// the export went. An export that failed never undoes the close: the database is
/// the Plan's source of truth, and the line says where the text did not go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCloseReport {
    pub cause: PlanCloseCause,
    pub number: i64,
    pub result: PlanResult,
    /// The file written, or why none was.
    pub export: Result<String, String>,
}

impl PlanCloseReport {
    /// What the operator is told. `None` for a disbanded brigade whose export went
    /// well: the disband has its own line, and a second one would only repeat it.
    pub fn status_line(&self) -> Option<String> {
        let ending = self.result.as_token();
        match (&self.export, self.cause) {
            (Ok(path), PlanCloseCause::Operator) => Some(format!(
                "Plan {} closed ({ending}) \u{2014} exported to {path}",
                self.number
            )),
            (Err(error), PlanCloseCause::Operator) => Some(format!(
                "Plan {} closed ({ending}); its export failed: {error}",
                self.number
            )),
            (Ok(_), PlanCloseCause::Disband) => None,
            (Err(error), PlanCloseCause::Disband) => Some(format!(
                "Plan {} was closed with the brigade, but its export failed: {error}",
                self.number
            )),
        }
    }
}

/// The message banto writes to the Director when the Planner releases a version.
pub fn release_notice(number: i64, version: i64, summary: &str) -> String {
    format!(
        "The Planner released Plan {number}, version {version} \u{2014} {summary}. It is a \
         change at the Planner's discretion inside what the operator approved (splitting or \
         reordering tasks), not a new approval, and it starts no review of its own. Call \
         read_plan to see what is now in force and bring Progress into line with it. If a \
         review of this Plan is already out, build nothing until it is answered."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{Progress, ProgressTask};

    fn plan(state: PlanState) -> Plan {
        Plan {
            number: 3,
            title: "Layout".to_string(),
            request: "make the panes draggable".to_string(),
            state,
            result: None,
            closed_by: None,
            opened_at_ms: 1,
            closed_at_ms: None,
        }
    }

    fn version(n: i64, kind: VersionKind) -> PlanVersion {
        PlanVersion {
            version: n,
            body: format!("BODY-{n}"),
            summary: format!("summary {n}"),
            kind,
            created_at_ms: n,
            approved_at_ms: (kind == VersionKind::OperatorApproved).then_some(n),
        }
    }

    fn progress(states: &[(&str, TaskState)]) -> Progress {
        Progress {
            stage: "impl".to_string(),
            tasks: states
                .iter()
                .map(|(id, state)| ProgressTask {
                    id: id.to_string(),
                    state: *state,
                    note: String::new(),
                })
                .collect(),
            off_plan: Vec::new(),
        }
    }

    #[test]
    fn tokens_round_trip_and_nothing_near_them_parses() {
        for state in [PlanState::Drafting, PlanState::Approved, PlanState::Closed] {
            assert_eq!(PlanState::from_token(state.as_token()), Some(state));
        }
        for result in [PlanResult::Completed, PlanResult::Abandoned] {
            assert_eq!(PlanResult::from_token(result.as_token()), Some(result));
        }
        for closer in [PlanCloser::Director, PlanCloser::Operator] {
            assert_eq!(PlanCloser::from_token(closer.as_token()), Some(closer));
        }
        for kind in [
            VersionKind::Draft,
            VersionKind::OperatorApproved,
            VersionKind::PlannerReleased,
        ] {
            assert_eq!(VersionKind::from_token(kind.as_token()), Some(kind));
        }
        assert_eq!(PlanState::from_token("Approved"), None);
        assert_eq!(PlanResult::from_token("done"), None);
        assert_eq!(VersionKind::from_token("approved"), None);
    }

    #[test]
    fn only_a_draft_is_not_in_force() {
        assert!(!VersionKind::Draft.in_force());
        assert!(VersionKind::OperatorApproved.in_force());
        assert!(VersionKind::PlannerReleased.in_force());
    }

    #[test]
    fn a_write_keeps_the_bodys_lines_and_folds_the_summary() {
        let (body, summary) = check_write(
            "# 仕様\n\n- T1 ドラッグ\n  - 詳細\r\n",
            " 一行目\n二行目\u{1b}[0m ",
        )
        .unwrap();
        assert_eq!(body, "# 仕様\n\n- T1 ドラッグ\n  - 詳細");
        assert_eq!(summary, "一行目 二行目[0m");
    }

    #[test]
    fn a_write_with_nothing_visible_or_too_much_is_refused_and_not_cut() {
        assert_eq!(
            check_write(" \n\u{200b}", "s"),
            Err(PlanRejection::NothingVisible { what: "body" })
        );
        assert_eq!(
            check_write("b", "\n"),
            Err(PlanRejection::NothingVisible { what: "summary" })
        );
        let long = "a".repeat(PLAN_BODY_MAX_CHARS + 1);
        assert_eq!(
            check_write(&long, "s"),
            Err(PlanRejection::TooLong {
                what: "body",
                limit: PLAN_BODY_MAX_CHARS,
                chars: PLAN_BODY_MAX_CHARS + 1
            })
        );
        assert!(check_write(&"a".repeat(PLAN_BODY_MAX_CHARS), "s").is_ok());
        let long_summary = "s".repeat(PLAN_SUMMARY_MAX_CHARS + 1);
        assert!(matches!(
            check_write("b", &long_summary),
            Err(PlanRejection::TooLong {
                what: "summary",
                ..
            })
        ));
        let said = check_write(&long, "s").unwrap_err().to_string();
        assert!(said.contains("Nothing was written"), "{said}");
    }

    #[test]
    fn a_request_is_required_and_known_is_not() {
        assert!(check_request("  \n").is_err());
        assert_eq!(check_request("do it\n\n\nnow").unwrap(), "do it\n\nnow");
        assert_eq!(check_known("  ").unwrap(), None);
        assert_eq!(check_known("a").unwrap().as_deref(), Some("a"));
        assert!(check_known(&"a".repeat(PLAN_REQUEST_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn completed_needs_an_approved_plan_and_tasks_all_closed() {
        let done = progress(&[("T1", TaskState::Done), ("T2", TaskState::Dropped)]);
        assert_eq!(completion_refusal(PlanState::Approved, Some(&done)), None);
        assert_eq!(
            completion_refusal(PlanState::Drafting, Some(&done)),
            Some(CompletionRefusal::NeverApproved)
        );
        assert_eq!(
            completion_refusal(PlanState::Approved, None),
            Some(CompletionRefusal::NoTasks)
        );
        assert_eq!(
            completion_refusal(PlanState::Approved, Some(&progress(&[]))),
            Some(CompletionRefusal::NoTasks)
        );
        for open in [TaskState::Todo, TaskState::Doing, TaskState::Blocked] {
            let some_open = progress(&[("T1", TaskState::Done), ("T2", open)]);
            assert_eq!(
                completion_refusal(PlanState::Approved, Some(&some_open)),
                Some(CompletionRefusal::OpenTasks(vec!["T2".to_string()])),
                "{open:?}"
            );
        }
    }

    #[test]
    fn the_operators_close_is_completed_only_when_completed_would_be_allowed() {
        let done = progress(&[("T1", TaskState::Done)]);
        let open = progress(&[("T1", TaskState::Doing)]);
        assert_eq!(
            operator_close_result(PlanState::Approved, Some(&done)),
            PlanResult::Completed
        );
        assert_eq!(
            operator_close_result(PlanState::Approved, Some(&open)),
            PlanResult::Abandoned
        );
        assert_eq!(
            operator_close_result(PlanState::Drafting, Some(&done)),
            PlanResult::Abandoned
        );
    }

    #[test]
    fn nobody_but_the_planner_gets_a_drafts_body() {
        let view = PlanView {
            plan: plan(PlanState::Approved),
            in_force: Some(version(1, VersionKind::OperatorApproved)),
            pending: Some(version(2, VersionKind::Draft)),
        };
        for role in [
            BrigadeRole::Director,
            BrigadeRole::Worker,
            BrigadeRole::Goinkyo,
        ] {
            let text = read_plan_text(role, Some(&view));
            assert!(text.contains("BODY-1"), "{role:?}: {text}");
            assert!(!text.contains("BODY-2"), "{role:?}: {text}");
            assert!(
                text.contains("A newer draft awaits the operator's approval"),
                "{role:?}: {text}"
            );
        }
        let planner = read_plan_text(BrigadeRole::Planner, Some(&view));
        assert!(
            planner.contains("BODY-1") && planner.contains("BODY-2"),
            "{planner}"
        );
        assert!(planner.contains("version 2 (not approved"), "{planner}");
    }

    #[test]
    fn before_any_approval_others_are_told_there_is_nothing_to_act_on() {
        let view = PlanView {
            plan: plan(PlanState::Drafting),
            in_force: None,
            pending: Some(version(1, VersionKind::Draft)),
        };
        let worker = read_plan_text(BrigadeRole::Worker, Some(&view));
        assert!(!worker.contains("BODY-1"), "{worker}");
        assert!(worker.contains("Drafting, no approved version"), "{worker}");
        let planner = read_plan_text(BrigadeRole::Planner, Some(&view));
        assert!(planner.contains("BODY-1"), "{planner}");
        // Not even a first draft yet.
        let empty = PlanView {
            plan: plan(PlanState::Drafting),
            in_force: None,
            pending: None,
        };
        let text = read_plan_text(BrigadeRole::Director, Some(&empty));
        assert!(text.contains("Drafting, no approved version"), "{text}");
    }

    #[test]
    fn a_release_is_said_to_be_the_planners_and_an_approval_the_operators() {
        let released = PlanView {
            plan: plan(PlanState::Approved),
            in_force: Some(version(2, VersionKind::PlannerReleased)),
            pending: None,
        };
        let text = read_plan_text(BrigadeRole::Director, Some(&released));
        assert!(text.contains("released by the Planner"), "{text}");
        assert!(!text.contains("awaits"), "{text}");
        let approved = PlanView {
            plan: plan(PlanState::Approved),
            in_force: Some(version(1, VersionKind::OperatorApproved)),
            pending: None,
        };
        assert!(
            read_plan_text(BrigadeRole::Worker, Some(&approved))
                .contains("approved by the operator")
        );
    }

    #[test]
    fn no_plan_is_said_so() {
        assert_eq!(
            read_plan_text(BrigadeRole::Worker, None),
            "This brigade has no open Plan."
        );
    }

    /// A frame of fixed width for the splitting tests: the budget arithmetic is
    /// then plain, and the real header and footer have their own test.
    fn frame_header(part: usize, of: usize) -> String {
        format!("<{part}/{of}>\n")
    }

    fn frame_footer(part: usize) -> String {
        format!("\n(next {})\n", part + 1)
    }

    fn split(text: &str, budget: usize) -> Vec<String> {
        split_into_parts(text, budget, frame_header, frame_footer)
    }

    /// The pieces of `parts` with every header and footer taken off, joined.
    fn unframed(parts: &[String]) -> String {
        let of = parts.len();
        parts
            .iter()
            .enumerate()
            .map(|(index, part)| {
                let part_no = index + 1;
                let rest = part
                    .strip_prefix(&frame_header(part_no, of))
                    .unwrap_or_else(|| panic!("part {part_no} has no header: {part:?}"));
                if part_no < of {
                    rest.strip_suffix(&frame_footer(part_no))
                        .unwrap_or_else(|| panic!("part {part_no} has no footer: {part:?}"))
                } else {
                    assert!(
                        !rest.contains("(next"),
                        "the last part has a footer: {part:?}"
                    );
                    rest
                }
            })
            .collect()
    }

    #[test]
    fn a_view_that_fits_is_one_part_and_is_returned_as_it_is() {
        let text = "line one\nline two\n";
        assert_eq!(split(text, text.len()), vec![text.to_string()]);
        assert_eq!(split("", 10), vec![String::new()]);
        let view = PlanView {
            plan: plan(PlanState::Approved),
            in_force: Some(version(1, VersionKind::OperatorApproved)),
            pending: None,
        };
        assert_eq!(
            read_plan_parts(BrigadeRole::Worker, Some(&view), 32_000),
            vec![read_plan_text(BrigadeRole::Worker, Some(&view))]
        );
        assert_eq!(
            read_plan_parts(BrigadeRole::Worker, None, 5),
            vec!["This brigade has no open Plan.".to_string()],
            "the words about there being no Plan are not cut"
        );
    }

    #[test]
    fn one_byte_over_the_budget_makes_two_parts_each_within_it() {
        let line = "0123456789\n";
        let text = line.repeat(4);
        // Room for exactly two lines after the frame of a one-digit part.
        let frame = frame_header(9, 9).len() + frame_footer(9).len();
        let budget = frame + 2 * line.len();
        let parts = split(&text, budget);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts.iter().all(|part| part.len() <= budget), "{parts:?}");
        assert_eq!(unframed(&parts), text);
        assert_eq!(parts[0], format!("<1/2>\n{}\n(next 2)\n", line.repeat(2)));

        // The text itself at the budget is one part; one byte less is not.
        assert_eq!(split(&text, text.len()).len(), 1);
        assert!(split(&text, text.len() - 1).len() > 1);
    }

    #[test]
    fn japanese_text_is_counted_in_bytes_and_cut_between_lines() {
        let line = "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{884c}\u{3067}\u{3059}\u{3002}\n";
        assert_eq!(line.len(), 25);
        let text = line.repeat(10);
        let budget = 100;
        let parts = split(&text, budget);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|part| part.len() <= budget), "{parts:?}");
        for part in &parts {
            let body_start = part.find('\n').unwrap() + 1;
            assert!(
                part[body_start..]
                    .lines()
                    .all(|l| l.is_empty() || l.starts_with('\u{65e5}') || l.starts_with('(')),
                "a line was cut: {part:?}"
            );
        }
        assert_eq!(unframed(&parts), text);
    }

    #[test]
    fn a_line_longer_than_a_part_is_cut_on_a_character_boundary() {
        // A three-byte character, with the cut landing on each of its bytes.
        for lead in 0..3 {
            let text = format!("{}{}\ntail\n", "a".repeat(lead), "\u{3042}".repeat(60));
            let parts = split(&text, 60);
            assert!(parts.len() > 2, "{parts:?}");
            assert!(parts.iter().all(|part| part.len() <= 60), "{parts:?}");
            assert_eq!(unframed(&parts), text, "lead {lead}");
        }
        // What is left of a cut line goes on being filled by the lines after it.
        let text = format!("{}\nshort\n", "x".repeat(100));
        let parts = split(&text, 70);
        assert_eq!(unframed(&parts), text);
        assert!(parts.last().unwrap().contains("short"), "{parts:?}");
    }

    #[test]
    fn every_part_but_the_last_ends_with_the_footer_that_names_the_next() {
        let text = "row\n".repeat(200);
        let parts = split(&text, 120);
        assert!(parts.len() > 3);
        for (index, part) in parts.iter().enumerate() {
            let part_no = index + 1;
            assert!(part.starts_with(&frame_header(part_no, parts.len())));
            assert_eq!(
                part.ends_with(&frame_footer(part_no)),
                part_no < parts.len(),
                "part {part_no}"
            );
        }
    }

    #[test]
    fn the_room_set_aside_follows_the_digits_of_the_part_count() {
        // Past nine parts the numbers have two digits and the frame grows: no part
        // may then exceed the budget, and the count in the headers must be right.
        let text = "row\n".repeat(400);
        let parts = split(&text, 60);
        assert!(parts.len() > 9, "{}", parts.len());
        assert!(parts.iter().all(|part| part.len() <= 60), "{parts:?}");
        assert_eq!(unframed(&parts), text);
    }

    #[test]
    fn the_header_names_the_plan_and_the_versions_the_text_shows() {
        assert_eq!(
            part_header(3, Some(5), None, 2, 4),
            "[banto] Plan 3, version in force 5, part 2 of 4\n\n"
        );
        assert_eq!(
            part_header(3, Some(5), Some(6), 1, 4),
            "[banto] Plan 3, version in force 5, draft 6, part 1 of 4\n\n"
        );
        assert_eq!(
            part_header(3, None, Some(1), 1, 2),
            "[banto] Plan 3, version in force none, draft 1, part 1 of 2\n\n"
        );
        assert_eq!(
            part_footer(2),
            "\n[banto] More of this Plan follows: call read_plan with part = 3.\n"
        );
    }

    #[test]
    fn a_long_view_is_cut_with_the_header_of_what_its_reader_is_shown() {
        let mut in_force = version(1, VersionKind::OperatorApproved);
        in_force.body = "spec line\n".repeat(120);
        let mut draft = version(2, VersionKind::Draft);
        draft.body = "draft line\n".repeat(120);
        let view = PlanView {
            plan: plan(PlanState::Approved),
            in_force: Some(in_force),
            pending: Some(draft),
        };
        let budget = 400;
        let planner = read_plan_parts(BrigadeRole::Planner, Some(&view), budget);
        let worker = read_plan_parts(BrigadeRole::Worker, Some(&view), budget);
        assert!(planner.len() > 1 && worker.len() > 1);
        assert!(
            planner
                .iter()
                .chain(&worker)
                .all(|part| part.len() <= budget)
        );
        assert!(
            planner[0].starts_with("[banto] Plan 3, version in force 1, draft 2, part 1 of "),
            "{:?}",
            planner[0]
        );
        assert!(
            worker[0].starts_with("[banto] Plan 3, version in force 1, part 1 of "),
            "{:?}",
            worker[0]
        );
        let planner_all = planner.concat();
        assert_eq!(planner_all.matches("spec line").count(), 120);
        assert_eq!(planner_all.matches("draft line").count(), 120);
        assert_eq!(worker.concat().matches("draft line").count(), 0);
    }

    #[test]
    fn the_export_shows_the_version_in_force_and_never_a_pending_drafts_body() {
        let view = PlanView {
            plan: plan(PlanState::Approved),
            in_force: Some(version(1, VersionKind::OperatorApproved)),
            pending: Some(version(2, VersionKind::Draft)),
        };
        let out = render_export(7, &view, None);
        assert!(out.contains("# Plan 3"), "{out}");
        assert!(out.contains("State: approved"), "{out}");
        assert!(out.contains("BODY-1") && !out.contains("BODY-2"), "{out}");
        assert!(out.contains("never approved and is not included"), "{out}");
        assert!(!out.contains("Progress at the end"), "{out}");
    }

    #[test]
    fn the_export_of_a_plan_never_approved_marks_the_draft_as_one() {
        let view = PlanView {
            plan: plan(PlanState::Drafting),
            in_force: None,
            pending: Some(version(1, VersionKind::Draft)),
        };
        let out = render_export(7, &view, None);
        assert!(out.contains("DRAFT, version 1 (never approved)"), "{out}");
        assert!(out.contains("BODY-1"), "{out}");
    }

    #[test]
    fn a_closed_export_says_how_it_ended_and_ends_with_the_final_progress() {
        let mut closed = plan(PlanState::Closed);
        closed.result = Some(PlanResult::Completed);
        closed.closed_by = Some(PlanCloser::Director);
        let view = PlanView {
            plan: closed,
            in_force: Some(version(1, VersionKind::OperatorApproved)),
            pending: None,
        };
        let mut done = progress(&[("T1", TaskState::Done)]);
        done.tasks[0].note = "merged".to_string();
        done.off_plan = vec!["fixed a typo".to_string()];
        let out = render_export(7, &view, Some(&done));
        assert!(
            out.contains("State: closed (completed by the director)"),
            "{out}"
        );
        assert!(out.contains("## Progress at the end"), "{out}");
        assert!(out.contains("- T1 [done] merged"), "{out}");
        assert!(out.contains("Outside the Plan:\n- fixed a typo"), "{out}");
    }

    #[test]
    fn the_notices_are_banto_words_that_name_the_plan_and_version() {
        let approval = approval_notice(3, 2, Some(GoinkyoConsult::Design));
        assert!(approval.contains("Plan 3") && approval.contains("version 2"));
        assert!(approval.contains("read_plan") && approval.contains("update_progress"));
        let release = release_notice(3, 4, "split T2");
        assert!(release.contains("Plan 3") && release.contains("version 4"));
        assert!(release.contains("split T2") && release.contains("not a new approval"));
        assert!(release.contains("no review of its own") && release.contains("build nothing"));
    }

    #[test]
    fn the_approval_notice_says_what_each_consultation_level_does_with_a_plan() {
        let text = |level| approval_notice(1, 1, level);
        for level in [
            None,
            Some(GoinkyoConsult::Off),
            Some(GoinkyoConsult::Disputes),
        ] {
            let t = text(level);
            assert!(t.contains("only if the operator asks"), "{level:?}: {t}");
            assert!(!t.contains("before the first errand"), "{level:?}: {t}");
        }
        let design = text(Some(GoinkyoConsult::Design));
        assert!(
            design.contains("If it contains a lasting change"),
            "{design}"
        );
        assert!(design.contains("otherwise no review is needed"), "{design}");
        let decisions = text(Some(GoinkyoConsult::Decisions));
        assert!(
            decisions.contains("Send it to the Goinkyo with kind"),
            "{decisions}"
        );
        assert!(!decisions.contains("lasting change"), "{decisions}");
        // Whatever the level, a review in flight blocks the build and its findings
        // have the same two homes, with scope also going to the operator.
        for level in [
            None,
            Some(GoinkyoConsult::Off),
            Some(GoinkyoConsult::Disputes),
            Some(GoinkyoConsult::Design),
            Some(GoinkyoConsult::Decisions),
        ] {
            let t = text(level);
            assert!(t.contains("build nothing"), "{level:?}: {t}");
            assert!(
                t.contains("to the Planner with send_to_peer and to Progress"),
                "{level:?}: {t}"
            );
            assert!(
                t.contains("also go to the operator with request_decision"),
                "{level:?}: {t}"
            );
        }
    }
}
