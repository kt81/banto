//! What a Director writes down about the state of its brigade's work, and the
//! rule that decides whether a write is accepted. Pure.
//!
//! The Director's own pane used to be the only place the state of the work
//! lived, and it filled with relayed reports the operator had to scroll through.
//! Progress is the place that state has instead: one per brigade, owned by the
//! Director, **replaced whole** on every write (it shows the latest state; it is
//! not a log). It is structured, not free text, so that banto can draw it as a
//! stage line, a task list and an off-plan list without parsing prose.
//!
//! The same module holds the rule for the question a Director attaches to
//! `request_decision`, since both are text an agent wrote that banto will draw
//! over several lines and so cannot fold to one the way a title is.
//!
//! The one gate: [`check_progress`] and [`check_question`] are the only way a
//! [`Progress`] or a stored question is made, so the tool that refuses a bad
//! write and the store that holds a good one cannot disagree about what is bad.
//! Limits count characters (Unicode scalar values), not columns: this is a bound
//! on what is stored, and a row of Japanese costs the same to keep as a row of
//! Latin letters; the screen wraps by display width on its own.

use serde::{Deserialize, Serialize};

use crate::display_text::{display_lines, display_name};

/// The longest `stage`, in characters, for a write. It is one line on the Director's
/// tile, which is as wide as a terminal pane; a stored one over the limit (written
/// under an older, longer limit) is still read and drawn, cut by width.
pub const STAGE_MAX_CHARS: usize = 80;
/// The longest task `id`, in characters. An id is a handle (`T1`), not a title.
pub const TASK_ID_MAX_CHARS: usize = 16;
/// The longest task `note`, in characters.
pub const TASK_NOTE_MAX_CHARS: usize = 200;
/// The longest `off_plan` item, in characters.
pub const OFF_PLAN_ITEM_MAX_CHARS: usize = 200;
/// The most tasks one Progress holds.
pub const TASKS_MAX: usize = 40;
/// The most `off_plan` items one Progress holds.
pub const OFF_PLAN_MAX: usize = 20;
/// The longest decision question, in characters, line breaks included.
pub const QUESTION_MAX_CHARS: usize = 4000;

/// Where a task stands. banto's own words, fixed: the screen draws them and a
/// reader of the store can rely on them, which an agent's own vocabulary
/// (`in progress`, `wip`, `finished`) would not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Todo,
    Doing,
    Blocked,
    Done,
    Dropped,
}

impl TaskState {
    /// Every word, in the order the tool's description lists them.
    pub const ALL: [TaskState; 5] = [
        TaskState::Todo,
        TaskState::Doing,
        TaskState::Blocked,
        TaskState::Done,
        TaskState::Dropped,
    ];

    /// The word the tool takes and the screen draws.
    pub fn as_token(self) -> &'static str {
        match self {
            TaskState::Todo => "todo",
            TaskState::Doing => "doing",
            TaskState::Blocked => "blocked",
            TaskState::Done => "done",
            TaskState::Dropped => "dropped",
        }
    }

    /// The state a word names; exactly the five, in lower case, nothing near
    /// them: a word that is almost one is a mistake to report, not to guess at.
    pub fn from_token(word: &str) -> Option<TaskState> {
        TaskState::ALL
            .into_iter()
            .find(|state| state.as_token() == word)
    }
}

/// One line of a Progress's task list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressTask {
    pub id: String,
    pub state: TaskState,
    /// One line; may be empty.
    pub note: String,
}

/// The state of a brigade's work, as its Director last wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Progress {
    /// One line: what phase the work is in.
    pub stage: String,
    pub tasks: Vec<ProgressTask>,
    /// Work being done that is outside what was agreed, one line each.
    pub off_plan: Vec<String>,
}

/// A stored Progress with what the store knows about the write. What the reading
/// screen draws.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressRecord {
    pub progress: Progress,
    /// How many writes this brigade's Progress has had; the tick reads this alone
    /// ([`ProgressGlance`]) to tell that the body changed.
    pub seq: i64,
    /// banto's clock at the write, in unix milliseconds.
    pub updated_at_ms: i64,
}

/// The part of a stored Progress the tick reads for every brigade: the write
/// count, which says whether the body changed, and the stage line with its write
/// time, which the Director's tile draws without the body being read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressGlance {
    pub seq: i64,
    pub stage: String,
    /// banto's clock at the write, in unix milliseconds: the age the tile shows is
    /// worked out against the drawing frame's own "now", so the core reads no clock.
    /// Zero in a stream recorded before it was carried, which reads as written long
    /// ago.
    #[serde(default)]
    pub updated_at_ms: i64,
}

/// What the reading screen shows about a brigade, read from the store when the
/// screen opens and again whenever the tick says one of its two parts changed.
/// Ages are computed by the shell that read the rows, so that the core never
/// reads a wall clock (the same reason `InterruptFact::age_ms` is carried).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrigadeReading {
    /// The Director's open request, if the store holds one.
    pub decision: Option<DecisionReading>,
    /// The Director's Progress, if it has written one.
    pub progress: Option<ProgressReading>,
    /// The brigade's open Plan, if it has one. Boxed because it carries two whole
    /// versions, and this struct travels inside `Event` and `Modal`, whose other
    /// variants are small.
    #[serde(default)]
    pub plan: Option<Box<crate::plan::PlanReading>>,
}

/// The open request as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionReading {
    pub seq: i64,
    /// The sanitized question the Director passed, `None` when it passed none
    /// and wrote it only in its own pane.
    pub question: Option<String>,
    /// How long ago the request was made, `None` for a row written before
    /// requests carried a time.
    pub age_ms: Option<i64>,
}

/// A stored Progress with how long ago it was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressReading {
    pub progress: Progress,
    pub seq: i64,
    pub age_ms: i64,
}

/// A task as the caller wrote it, before the rule has looked at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTask {
    pub id: String,
    pub state: String,
    pub note: String,
}

/// Why a write was refused, worded for the Director that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressRejection {
    /// `stage` has nothing visible once reduced.
    StageEmpty,
    TooLong {
        what: String,
        limit: usize,
        chars: usize,
    },
    TooMany {
        what: &'static str,
        limit: usize,
        count: usize,
    },
    /// Task `n` (1-based) has an id with nothing visible in it.
    TaskIdEmpty {
        n: usize,
    },
    TaskStateUnknown {
        n: usize,
        word: String,
    },
    TaskIdRepeated {
        id: String,
    },
    OffPlanEmpty {
        n: usize,
    },
}

impl std::fmt::Display for ProgressRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProgressRejection::StageEmpty => write!(
                f,
                "`stage` has nothing visible in it. Say in one line what phase the work is in."
            ),
            ProgressRejection::TooLong { what, limit, chars } => write!(
                f,
                "{what} is {chars} characters; the limit is {limit}. Shorten it: Progress is \
                 one line per item, not a report."
            ),
            ProgressRejection::TooMany { what, limit, count } => write!(
                f,
                "{count} {what}; the limit is {limit}. Drop the ones that no longer matter \
                 (each write replaces the whole Progress)."
            ),
            ProgressRejection::TaskIdEmpty { n } => {
                write!(
                    f,
                    "task {n} has an empty `id`. Give it a short handle such as T1."
                )
            }
            ProgressRejection::TaskStateUnknown { n, word } => write!(
                f,
                "task {n} has the state {word:?}; the words are {}.",
                TaskState::ALL.map(TaskState::as_token).join(", ")
            ),
            ProgressRejection::TaskIdRepeated { id } => {
                write!(f, "the task id {id:?} is used twice; ids must be unique.")
            }
            ProgressRejection::OffPlanEmpty { n } => write!(
                f,
                "`off_plan` item {n} has nothing visible in it. Leave an item out rather \
                 than send an empty one."
            ),
        }
    }
}

/// A one-line field reduced for storage: folded to one visible line by
/// [`display_name`]. `None` when nothing visible is left.
fn one_line(raw: &str) -> Option<String> {
    display_name(raw)
}

fn check_len(what: String, text: &str, limit: usize) -> Result<(), ProgressRejection> {
    let chars = text.chars().count();
    if chars > limit {
        return Err(ProgressRejection::TooLong { what, limit, chars });
    }
    Ok(())
}

/// What a write is checked as, and reduced to: the stage and every line folded
/// to one visible line, control and invisible characters dropped (the operator's
/// own language is kept as written), the counts and lengths within the limits,
/// the states one of banto's five words, the task ids unique.
///
/// Nothing is cut to fit: a write over a limit is refused, so the Director sees
/// that it was and writes a shorter one, rather than the operator reading a
/// sentence that stops.
pub fn check_progress(
    stage: &str,
    tasks: &[RawTask],
    off_plan: &[String],
) -> Result<Progress, ProgressRejection> {
    let stage = one_line(stage).ok_or(ProgressRejection::StageEmpty)?;
    check_len("`stage`".to_string(), &stage, STAGE_MAX_CHARS)?;
    if tasks.len() > TASKS_MAX {
        return Err(ProgressRejection::TooMany {
            what: "tasks",
            limit: TASKS_MAX,
            count: tasks.len(),
        });
    }
    if off_plan.len() > OFF_PLAN_MAX {
        return Err(ProgressRejection::TooMany {
            what: "off_plan items",
            limit: OFF_PLAN_MAX,
            count: off_plan.len(),
        });
    }
    let mut checked: Vec<ProgressTask> = Vec::with_capacity(tasks.len());
    for (index, task) in tasks.iter().enumerate() {
        let n = index + 1;
        let id = one_line(&task.id).ok_or(ProgressRejection::TaskIdEmpty { n })?;
        check_len(format!("task {n}'s `id`"), &id, TASK_ID_MAX_CHARS)?;
        if checked.iter().any(|earlier| earlier.id == id) {
            return Err(ProgressRejection::TaskIdRepeated { id });
        }
        let state = TaskState::from_token(&task.state).ok_or_else(|| {
            ProgressRejection::TaskStateUnknown {
                n,
                word: task.state.clone(),
            }
        })?;
        let note = one_line(&task.note).unwrap_or_default();
        check_len(format!("task {n}'s `note`"), &note, TASK_NOTE_MAX_CHARS)?;
        checked.push(ProgressTask { id, state, note });
    }
    let mut items = Vec::with_capacity(off_plan.len());
    for (index, item) in off_plan.iter().enumerate() {
        let n = index + 1;
        let item = one_line(item).ok_or(ProgressRejection::OffPlanEmpty { n })?;
        check_len(
            format!("`off_plan` item {n}"),
            &item,
            OFF_PLAN_ITEM_MAX_CHARS,
        )?;
        items.push(item);
    }
    Ok(Progress {
        stage,
        tasks: checked,
        off_plan: items,
    })
}

/// Why a decision question was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionTooLong {
    pub chars: usize,
}

impl std::fmt::Display for QuestionTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`question` is {} characters; the limit is {QUESTION_MAX_CHARS}. Put the decision \
             itself in it (what is being decided, the options, your recommendation) and keep \
             the background in your pane.",
            self.chars
        )
    }
}

/// A decision question reduced for storage: line breaks kept, control and
/// invisible characters dropped ([`display_lines`]). `Ok(None)` when nothing
/// visible is left, which is the same as sending no question.
pub fn check_question(input: &str) -> Result<Option<String>, QuestionTooLong> {
    let text = display_lines(input);
    if text.is_empty() {
        return Ok(None);
    }
    let chars = text.chars().count();
    if chars > QUESTION_MAX_CHARS {
        return Err(QuestionTooLong { chars });
    }
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, state: &str, note: &str) -> RawTask {
        RawTask {
            id: id.to_string(),
            state: state.to_string(),
            note: note.to_string(),
        }
    }

    #[test]
    fn a_valid_write_is_kept_as_written_japanese_included() {
        let progress = check_progress(
            "実装中: 保存層",
            &[task("T1", "doing", "マイグレーションを書く")],
            &["README の表を直している".to_string()],
        )
        .unwrap();
        assert_eq!(progress.stage, "実装中: 保存層");
        assert_eq!(progress.tasks[0].id, "T1");
        assert_eq!(progress.tasks[0].state, TaskState::Doing);
        assert_eq!(progress.tasks[0].note, "マイグレーションを書く");
        assert_eq!(progress.off_plan, ["README の表を直している"]);
    }

    #[test]
    fn tasks_and_off_plan_may_be_empty_but_the_stage_may_not() {
        let progress = check_progress("planning", &[], &[]).unwrap();
        assert!(progress.tasks.is_empty() && progress.off_plan.is_empty());
        assert_eq!(
            check_progress("  \u{200b} \n ", &[], &[]),
            Err(ProgressRejection::StageEmpty)
        );
    }

    #[test]
    fn every_line_is_folded_to_one_visible_line_and_controls_are_dropped() {
        let progress = check_progress(
            "phase\n two\u{1b}[31m",
            &[task("T\n1", "todo", "a\tb\r\nc")],
            &["x\u{202e}y".to_string()],
        )
        .unwrap();
        assert_eq!(progress.stage, "phase two[31m");
        assert_eq!(progress.tasks[0].id, "T 1");
        assert_eq!(progress.tasks[0].note, "a b c");
        assert_eq!(progress.off_plan, ["xy"]);
    }

    #[test]
    fn a_state_is_one_of_the_five_words_and_nothing_near_them() {
        for state in TaskState::ALL {
            assert_eq!(TaskState::from_token(state.as_token()), Some(state));
        }
        for word in ["Done", "in progress", "wip", "", " done"] {
            assert_eq!(TaskState::from_token(word), None, "{word:?}");
        }
        let error = check_progress("s", &[task("T1", "wip", "")], &[]).unwrap_err();
        let said = error.to_string();
        assert!(said.contains("task 1"), "{said}");
        assert!(
            said.contains("todo, doing, blocked, done, dropped"),
            "{said}"
        );
    }

    #[test]
    fn a_state_is_stored_as_its_word() {
        let json = serde_json::to_string(&TaskState::Blocked).unwrap();
        assert_eq!(json, "\"blocked\"");
        assert_eq!(
            serde_json::from_str::<TaskState>("\"dropped\"").unwrap(),
            TaskState::Dropped
        );
    }

    #[test]
    fn a_recorded_glance_without_the_write_time_still_parses() {
        let old: ProgressGlance = serde_json::from_str(r#"{"seq":3,"stage":"s"}"#).unwrap();
        assert_eq!(
            old,
            ProgressGlance {
                seq: 3,
                stage: "s".to_string(),
                updated_at_ms: 0,
            }
        );
        let new = ProgressGlance {
            seq: 4,
            stage: "実装".to_string(),
            updated_at_ms: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&new).unwrap();
        assert_eq!(serde_json::from_str::<ProgressGlance>(&json).unwrap(), new);
    }

    #[test]
    fn the_limits_refuse_rather_than_cut() {
        let long = "あ".repeat(STAGE_MAX_CHARS + 1);
        assert!(matches!(
            check_progress(&long, &[], &[]),
            Err(ProgressRejection::TooLong {
                limit: STAGE_MAX_CHARS,
                ..
            })
        ));
        // At the limit is fine, and counted in characters, not bytes.
        assert!(check_progress(&"あ".repeat(STAGE_MAX_CHARS), &[], &[]).is_ok());
        let many: Vec<RawTask> = (0..=TASKS_MAX)
            .map(|n| task(&format!("T{n}"), "todo", ""))
            .collect();
        assert!(matches!(
            check_progress("s", &many, &[]),
            Err(ProgressRejection::TooMany { what: "tasks", .. })
        ));
        assert!(check_progress("s", &many[..TASKS_MAX], &[]).is_ok());
        let off: Vec<String> = (0..=OFF_PLAN_MAX).map(|n| format!("item {n}")).collect();
        assert!(matches!(
            check_progress("s", &[], &off),
            Err(ProgressRejection::TooMany { .. })
        ));
        assert!(matches!(
            check_progress(
                "s",
                &[task(&"i".repeat(TASK_ID_MAX_CHARS + 1), "todo", "")],
                &[]
            ),
            Err(ProgressRejection::TooLong {
                limit: TASK_ID_MAX_CHARS,
                ..
            })
        ));
        assert!(matches!(
            check_progress(
                "s",
                &[task("T1", "todo", &"n".repeat(TASK_NOTE_MAX_CHARS + 1))],
                &[]
            ),
            Err(ProgressRejection::TooLong {
                limit: TASK_NOTE_MAX_CHARS,
                ..
            })
        ));
        assert!(matches!(
            check_progress("s", &[], &["o".repeat(OFF_PLAN_ITEM_MAX_CHARS + 1)]),
            Err(ProgressRejection::TooLong {
                limit: OFF_PLAN_ITEM_MAX_CHARS,
                ..
            })
        ));
    }

    #[test]
    fn ids_must_be_visible_and_unique_and_off_plan_items_visible() {
        assert_eq!(
            check_progress("s", &[task(" ", "todo", "")], &[]),
            Err(ProgressRejection::TaskIdEmpty { n: 1 })
        );
        assert_eq!(
            check_progress("s", &[task("T1", "todo", ""), task("T1", "done", "")], &[]),
            Err(ProgressRejection::TaskIdRepeated {
                id: "T1".to_string()
            })
        );
        assert_eq!(
            check_progress("s", &[], &["ok".to_string(), "\u{200b}".to_string()]),
            Err(ProgressRejection::OffPlanEmpty { n: 2 })
        );
    }

    #[test]
    fn a_question_keeps_its_line_breaks_and_drops_what_steers_a_terminal() {
        let question = check_question("Ship it?\r\n\r\n\r\n- yes\n- no\u{1b}[2J \u{202e}").unwrap();
        assert_eq!(question.as_deref(), Some("Ship it?\n\n- yes\n- no[2J"));
    }

    #[test]
    fn a_question_with_nothing_visible_is_no_question_and_a_long_one_is_refused() {
        assert_eq!(check_question(" \n\u{200b}\t"), Ok(None));
        assert_eq!(check_question(""), Ok(None));
        let long = "q".repeat(QUESTION_MAX_CHARS + 1);
        assert_eq!(
            check_question(&long),
            Err(QuestionTooLong {
                chars: QUESTION_MAX_CHARS + 1
            })
        );
        assert!(
            check_question(&"q".repeat(QUESTION_MAX_CHARS))
                .unwrap()
                .is_some()
        );
    }
}
