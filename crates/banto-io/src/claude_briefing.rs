//! Whether a resumed Claude brigade member will see the briefing banto is
//! about to pass it with `--append-system-prompt`.
//!
//! Measured against Claude Code 2.1.326: Claude records the system prompt on a
//! conversation's first request, as an `attachment` record whose
//! `attachment.type` is `prompt_snapshot` and whose `attachment.systemPrompt`
//! is a list of text sections, banto's appended briefing being the last one.
//! Every later request, `--resume` included, reuses that recorded prompt, and a
//! resumed launch's own `--append-system-prompt` text is ignored — until a
//! compaction (a `system` record with `subtype` `compact_boundary`), after
//! which the next launch records its own text afresh. `--system-prompt-snapshot
//! off` makes Claude render the prompt on every request instead (the text then
//! reaches the model) but does not record it, so the flag is a bridge, not a
//! repair.
//!
//! The rule, from the end of the session's transcript walking complete lines
//! backwards to the first of:
//!
//! - a `prompt_snapshot`: its last section against the briefing about to be
//!   passed. Equal — nothing to do; different — pass the flag.
//! - a compact boundary: nothing to do, the next default launch records.
//! - anything that settles neither (a line that does not parse, a snapshot of a
//!   shape this reader does not know, the bound, the start of the file, a file
//!   that cannot be read): pass the flag. **Not finding a snapshot is never
//!   read as "there is none to disagree with"** — only a boundary newer than
//!   every snapshot proves that — so an upstream rename degrades to one
//!   redundant flag, never to a member that silently runs on a stale briefing.
//!
//! Records of any other type are walked past, and so are the records of a
//! subagent (`isSidechain: true`), whatever their type.
//!
//! The decision itself ([`scan_tail`]) is a pure function of bytes; only
//! [`decide`] touches the file. It lives here, beside the other transcript
//! readers, rather than in `banto-core`: it parses an agent's file format,
//! which is I/O-side knowledge (`docs/DISCIPLINE.md` §2). The transcript is only
//! ever read — it belongs to the agent's home.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

/// First window read from the end of the transcript. Measured on Claude Code
/// 2.1.326 in print mode: snapshots are written when a conversation is created
/// (before and after its first request) and on the first request after a
/// compaction; resumed turns wrote none. So the newest record that decides is
/// wherever the last of those happened, not near the end by construction. Four
/// real interactive transcripts of up to 127 MB had it never more than about
/// 12 MB from the end, which is why the window starts small and grows.
const START_WINDOW: u64 = 1 << 20;

/// The most of a transcript that is ever read, counted from the end: a resume
/// is on the operator's own startup path, and an unbounded read there is worse
/// than one redundant flag. A session with no snapshot or boundary within it
/// gets the flag, which is safe and at worst redundant.
const MAX_WINDOW: u64 = 32 << 20;

/// Stands for `{peers}` in a briefing rendered for comparison. A control
/// character no template contains, so splitting on it cannot find a hole the
/// operator did not write.
pub const PEERS_HOLE: &str = "\u{1f}banto-peers-hole\u{1f}";

/// How many hole ends one comparison may try before it gives up. Matching a
/// real roster against a real recording takes about a thousand; a transcript
/// built so that holes in a row can end almost anywhere takes far more, and the
/// launch must not wait on it.
const MATCH_BUDGET: usize = 1 << 19;

/// What may stand where a `{peers}` was. Given the text from the start of a
/// hole, returns the byte length of every prefix of it that a roster could have
/// rendered to, ascending, each on a character boundary, and looking no further
/// than a real roster reaches. Supplied by the composer, which owns that grammar
/// (`briefing::peers_ends`), so the two cannot drift.
///
/// Returning the ends, rather than judging one candidate text at a time, is what
/// keeps the search linear in what it reads: a judge called at every possible end
/// of a hole re-reads the whole prefix each time.
pub type HoleGrammar = fn(&str) -> Vec<usize>;

/// The briefing about to be passed, as the pieces around its `{peers}` holes.
///
/// `{peers}` names the members that exist when the briefing is rendered, which
/// is not the roster the recorded copy was rendered with: a Worker joined since.
/// A comparison that counted that as a change would put a flag on nearly every
/// resumed Director for no reason, so a hole matches a roster. It matches only
/// a roster ([`HoleGrammar`]), not any text: a hole that took anything would
/// also swallow a sentence the operator has since deleted from the template
/// right after `{peers}`, and the member would go on with the rule it was
/// meant to lose. Everything around a hole must match exactly.
#[derive(Debug, Clone)]
pub struct BriefingPattern {
    /// One more segment than there are holes; a lone segment is an exact text.
    segments: Vec<String>,
    hole: HoleGrammar,
}

impl BriefingPattern {
    /// A text with no hole: matched by equality.
    pub fn exact(text: &str) -> Self {
        Self {
            segments: vec![text.to_string()],
            hole: |_| Vec::new(),
        }
    }

    /// A text rendered with [`PEERS_HOLE`] in place of the roster. `banto`'s own
    /// composer produces it (`briefing::compose_pattern`), so the template logic
    /// lives in one place.
    pub fn from_marked(text: &str, hole: HoleGrammar) -> Self {
        Self {
            segments: text.split(PEERS_HOLE).map(str::to_string).collect(),
            hole,
        }
    }

    /// Whether `text` is this briefing for some roster. A comparison that
    /// would take more than [`MATCH_BUDGET`] steps says no: a roster is matched
    /// in a few thousand, so only a text built to be slow gets there, and a no
    /// costs a redundant flag, never a stale briefing.
    pub fn matches(&self, text: &str) -> bool {
        let mut budget = MATCH_BUDGET;
        self.matches_at(0, text, 0, &mut HashSet::new(), &mut budget)
    }

    /// Segment `index` at byte `pos` of `text`, and everything after it. Every
    /// end of the hole that follows is tried, nearest first, since the segment
    /// after it can occur more than once. A (segment, position) that failed once
    /// fails again however it was reached, so it is remembered: without that,
    /// holes in a row retry the same positions exponentially often, and a text
    /// built to do it would hold the launch for minutes.
    fn matches_at(
        &self,
        index: usize,
        text: &str,
        pos: usize,
        failed: &mut HashSet<(usize, usize)>,
        budget: &mut usize,
    ) -> bool {
        let Some(rest) = text
            .get(pos..)
            .and_then(|t| t.strip_prefix(self.segments[index].as_str()))
        else {
            return false;
        };
        let pos = text.len() - rest.len();
        if index + 1 == self.segments.len() {
            return rest.is_empty();
        }
        if failed.contains(&(index, pos)) {
            return false;
        }
        for end in (self.hole)(rest) {
            if *budget == 0 {
                return false;
            }
            *budget -= 1;
            if text.is_char_boundary(pos + end)
                && self.matches_at(index + 1, text, pos + end, failed, budget)
            {
                return true;
            }
        }
        failed.insert((index, pos));
        false
    }
}

/// What the walk found, and so whether the launch needs the flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The newest snapshot holds this briefing.
    Matches,
    /// A compaction is newer than any snapshot: the next launch records afresh.
    BoundaryNewer,
    /// The newest snapshot holds a different briefing.
    Differs,
    /// The newest snapshot has no last section that is text.
    UnknownShape,
    /// A complete line that is not a JSON object, or does not parse.
    Broken,
    /// The start of the transcript came with neither record.
    NoRecord,
    /// The bound came with neither record.
    BoundReached,
    /// No path, not a regular file, or an I/O error.
    Unreadable,
}

impl Outcome {
    /// `--system-prompt-snapshot off` is wanted for every outcome but the two
    /// that prove the default launch does the right thing.
    pub fn needs_snapshot_off(self) -> bool {
        !matches!(self, Outcome::Matches | Outcome::BoundaryNewer)
    }
}

/// The decision for one transcript. Never fails: every problem is an
/// [`Outcome`] that needs the flag. `None` is a session with no transcript to
/// read.
pub fn decide(transcript: Option<&Path>, briefing: &BriefingPattern) -> Outcome {
    decide_with(transcript, briefing, START_WINDOW, MAX_WINDOW)
}

fn decide_with(
    transcript: Option<&Path>,
    briefing: &BriefingPattern,
    start_window: u64,
    max_window: u64,
) -> Outcome {
    let Some(path) = transcript else {
        return Outcome::Unreadable;
    };
    read_and_scan(path, briefing, start_window, max_window).unwrap_or(Outcome::Unreadable)
}

/// Reads the transcript from the end in a growing window and scans it. `None`
/// for any I/O trouble. The same approach as `codex_briefing`'s reader; the two
/// are kept apart so neither's rule can leak into the other.
fn read_and_scan(
    path: &Path,
    briefing: &BriefingPattern,
    start_window: u64,
    max_window: u64,
) -> Option<Outcome> {
    // Opening a FIFO for reading blocks until someone writes to it.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut window = start_window;
    loop {
        let read_size = window.min(size);
        file.seek(SeekFrom::Start(size - read_size)).ok()?;
        let mut buf = vec![0u8; usize::try_from(read_size).ok()?];
        file.read_exact(&mut buf).ok()?;
        match scan_tail(briefing, &buf, read_size < size) {
            Scan::Undecided if window < max_window => {
                window = window.saturating_mul(4).min(max_window);
            }
            Scan::Undecided => return Some(Outcome::BoundReached),
            Scan::Decided(outcome) => return Some(outcome),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    Decided(Outcome),
    /// The window ran out before a decision and more of the file lies before
    /// it: a larger window may decide.
    Undecided,
}

/// Just enough of a record to route it; every other field is skipped without
/// being built. `type` and `subtype` are read loosely so an unexpected shape
/// there is an unknown record, which is walked past, not a parse failure.
#[derive(Deserialize)]
struct Head {
    #[serde(rename = "type")]
    kind: Option<Value>,
    subtype: Option<Value>,
    #[serde(rename = "isSidechain")]
    sidechain: Option<Value>,
}

/// Scans `tail` — the last bytes of a transcript, starting mid-file when
/// `starts_mid_file` — for the record that decides the question. The bytes after
/// the last `\n` are ignored: an unterminated line is the process that is still
/// writing it.
fn scan_tail(briefing: &BriefingPattern, tail: &[u8], starts_mid_file: bool) -> Scan {
    let nothing = if starts_mid_file {
        Scan::Undecided
    } else {
        Scan::Decided(Outcome::NoRecord)
    };
    let Some(end) = tail.iter().rposition(|&byte| byte == b'\n') else {
        return nothing;
    };
    let lines: Vec<&[u8]> = tail[..end].split(|&byte| byte == b'\n').collect();
    for (index, line) in lines.iter().enumerate().rev() {
        if index == 0 && starts_mid_file {
            // Cut by the window, not a line of its own: nothing it says counts.
            return Scan::Undecided;
        }
        if let Some(outcome) = judge_line(briefing, line) {
            return Scan::Decided(outcome);
        }
    }
    nothing
}

/// `Some` when this line settles the question, `None` to keep walking.
fn judge_line(briefing: &BriefingPattern, line: &[u8]) -> Option<Outcome> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    // A transcript line is always an object. Valid JSON of another kind means
    // this is not the file the reader thinks it is, and doubt takes the flag.
    if line.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Some(Outcome::Broken);
    }
    let Ok(head) = serde_json::from_slice::<Head>(line) else {
        return Some(Outcome::Broken);
    };
    // A subagent's records can sit in the same file, and its prompt is not the
    // conversation's: its snapshot must not decide for the main conversation,
    // nor its compaction for it. Only a literal `true` marks one; anything else
    // is read as the main conversation, where the worst a wrong guess does is
    // one redundant flag.
    if head.sidechain == Some(Value::Bool(true)) {
        return None;
    }
    match head.kind.as_ref().and_then(Value::as_str) {
        // Both strings match exactly: a looser test would read a record that
        // merely mentions a compaction as one.
        Some("system")
            if head.subtype.as_ref().and_then(Value::as_str) == Some("compact_boundary") =>
        {
            Some(Outcome::BoundaryNewer)
        }
        Some("attachment") => {
            let Ok(record) = serde_json::from_slice::<Value>(line) else {
                return Some(Outcome::Broken);
            };
            let attachment = record.get("attachment")?;
            if attachment.get("type").and_then(Value::as_str) != Some("prompt_snapshot") {
                return None;
            }
            Some(judge_snapshot(briefing, attachment))
        }
        _ => None,
    }
}

fn judge_snapshot(briefing: &BriefingPattern, attachment: &Value) -> Outcome {
    let last = attachment
        .get("systemPrompt")
        .and_then(Value::as_array)
        .and_then(|sections| sections.last())
        .and_then(Value::as_str);
    match last {
        Some(text) if briefing.matches(text) => Outcome::Matches,
        Some(_) => Outcome::Differs,
        None => Outcome::UnknownShape,
    }
}

/// Whether a resume of Claude session `session_id` needs
/// `--system-prompt-snapshot off` to deliver `briefing`: every transcript that
/// holds the id is read ([`find_transcripts`]) and the flag is wanted unless all
/// of them say it is not, and also when none is found.
///
/// Which copy Claude reads when more than one project folder holds the id has
/// not been measured, so none is trusted over the others: passing the flag
/// where the copy Claude used did not need it is redundant, while leaving it off
/// where it did leaves the member on a stale briefing.
pub fn resume_needs_snapshot_off(
    projects_dir: &Path,
    session_id: &str,
    briefing: &BriefingPattern,
) -> bool {
    let copies = find_transcripts(projects_dir, session_id);
    copies.is_empty()
        || copies
            .iter()
            .any(|path| decide(Some(path), briefing).needs_snapshot_off())
}

/// The transcripts of Claude session `session_id`:
/// `<projects>/<project>/<id>.jsonl`, the layout `provider::claude_code`
/// discovers sessions by (the id is the file stem). Empty when no project holds
/// it. More than one folder can (whether a session resumed from another
/// directory leaves a copy in that one has not been verified), so all are
/// returned, in no particular order.
///
/// The id is a path component here, so one that is not a plain token — it comes
/// out of banto's own store, but a path walk is the wrong place to trust that —
/// finds nothing.
pub fn find_transcripts(projects_dir: &Path, session_id: &str) -> Vec<std::path::PathBuf> {
    let plain = !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !plain {
        return Vec::new();
    }
    let file_name = format!("{session_id}.jsonl");
    let Ok(projects) = std::fs::read_dir(projects_dir) else {
        return Vec::new();
    };
    projects
        .filter_map(Result::ok)
        .map(|project| project.path().join(&file_name))
        .filter(|path| std::fs::metadata(path).is_ok_and(|meta| meta.is_file()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::TempDir;

    const BRIEFING: &str = "You are director in brigade 7.\n\nPeers: worker-1.";

    /// A snapshot as Claude Code 2.1.326 records it (synthetic; the keys were
    /// measured, the values are made up): the briefing is the last section.
    fn snapshot(last: &str) -> String {
        json!({
            "type": "attachment",
            "uuid": "00000000-0000-0000-0000-000000000001",
            "sessionId": "00000000-0000-0000-0000-000000000002",
            "attachment": {
                "type": "prompt_snapshot",
                "reminderFold": false,
                "contextRendering": "announced",
                "systemPrompt": ["You are an agent.", "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__", last],
            }
        })
        .to_string()
    }

    fn boundary() -> String {
        json!({
            "type": "system",
            "subtype": "compact_boundary",
            "content": "Conversation compacted",
            "compactMetadata": {"trigger": "manual", "preTokens": 3500},
        })
        .to_string()
    }

    fn other_attachment() -> String {
        json!({"type": "attachment", "attachment": {"type": "date", "date": "2026-01-01"}})
            .to_string()
    }

    fn unknown_record() -> String {
        json!({"type": "a_kind_claude_has_not_shipped_yet", "attachment": {"type": "prompt_snapshot"}})
            .to_string()
    }

    fn turn(n: usize) -> Vec<String> {
        vec![
            json!({"type": "user", "message": {"role": "user", "content": format!("{n:0>200}")}})
                .to_string(),
            json!({"type": "assistant", "message": {"role": "assistant", "content": format!("{n:0>200}")}})
                .to_string(),
        ]
    }

    fn write(lines: &[String]) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut text = lines.join("\n");
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn outcome(lines: &[String], briefing: &BriefingPattern) -> Outcome {
        let (_dir, path) = write(lines);
        decide(Some(&path), briefing)
    }

    fn exact() -> BriefingPattern {
        BriefingPattern::exact(BRIEFING)
    }

    #[test]
    fn a_snapshot_holding_the_briefing_needs_no_flag() {
        let result = outcome(&[snapshot(BRIEFING)], &exact());
        assert_eq!(result, Outcome::Matches);
        assert!(!result.needs_snapshot_off());
    }

    #[test]
    fn a_snapshot_holding_another_briefing_needs_the_flag() {
        let result = outcome(&[snapshot("an older briefing")], &exact());
        assert_eq!(result, Outcome::Differs);
        assert!(result.needs_snapshot_off());
    }

    #[test]
    fn a_snapshot_with_no_briefing_at_all_differs() {
        // A session that was never a brigade member: its last section is
        // Claude's own, and the briefing about to be passed is not in it.
        let lines = [json!({"type": "attachment", "attachment": {
            "type": "prompt_snapshot", "systemPrompt": ["Claude's own section."]}})
        .to_string()];
        assert_eq!(outcome(&lines, &exact()), Outcome::Differs);
    }

    #[test]
    fn the_newest_snapshot_decides_not_an_older_one() {
        let lines = [snapshot(BRIEFING), snapshot("changed since")];
        assert_eq!(outcome(&lines, &exact()), Outcome::Differs);
        let lines = [snapshot("changed since"), snapshot(BRIEFING)];
        assert_eq!(outcome(&lines, &exact()), Outcome::Matches);
    }

    #[test]
    fn a_boundary_newer_than_the_snapshot_needs_no_flag() {
        let lines = [snapshot("an older briefing"), boundary()];
        let result = outcome(&lines, &exact());
        assert_eq!(result, Outcome::BoundaryNewer);
        assert!(!result.needs_snapshot_off());
    }

    #[test]
    fn a_snapshot_newer_than_the_boundary_is_compared() {
        let lines = [boundary(), snapshot("recorded after"), other_attachment()];
        assert_eq!(outcome(&lines, &exact()), Outcome::Differs);
        let lines = [boundary(), snapshot(BRIEFING)];
        assert_eq!(outcome(&lines, &exact()), Outcome::Matches);
    }

    #[test]
    fn neither_record_needs_the_flag() {
        let mut lines = turn(1);
        lines.push(other_attachment());
        let result = outcome(&lines, &exact());
        assert_eq!(result, Outcome::NoRecord);
        assert!(result.needs_snapshot_off());
        // An empty file has neither record either.
        let (_dir, path) = write(&[]);
        assert_eq!(decide(Some(&path), &exact()), Outcome::NoRecord);
    }

    #[test]
    fn a_missing_or_unreadable_transcript_needs_the_flag() {
        assert_eq!(decide(None, &exact()), Outcome::Unreadable);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            decide(Some(&dir.path().join("absent.jsonl")), &exact()),
            Outcome::Unreadable
        );
        // A directory is not a transcript.
        assert_eq!(decide(Some(dir.path()), &exact()), Outcome::Unreadable);
        assert!(Outcome::Unreadable.needs_snapshot_off());
    }

    #[test]
    fn a_broken_line_before_a_decision_needs_the_flag() {
        for broken in [
            "{\"type\": \"attachment\", ",
            "not json at all",
            "[1, 2]",
            "42",
        ] {
            let lines = [snapshot(BRIEFING), broken.to_string()];
            let result = outcome(&lines, &exact());
            assert_eq!(result, Outcome::Broken, "{broken}");
            assert!(result.needs_snapshot_off());
        }
    }

    #[test]
    fn a_broken_line_beyond_the_decision_is_never_reached() {
        let lines = ["not json at all".to_string(), snapshot(BRIEFING)];
        assert_eq!(outcome(&lines, &exact()), Outcome::Matches);
    }

    #[test]
    fn blank_lines_are_walked_past() {
        let lines = [snapshot(BRIEFING), String::new(), "   ".to_string()];
        assert_eq!(outcome(&lines, &exact()), Outcome::Matches);
    }

    #[test]
    fn unknown_record_types_are_walked_past() {
        let mut lines = vec![snapshot(BRIEFING)];
        lines.push(unknown_record());
        lines.push(other_attachment());
        lines.extend(turn(1));
        lines.push(json!({"type": 7, "subtype": ["x"]}).to_string());
        lines.push(json!({"no_type_at_all": true}).to_string());
        assert_eq!(outcome(&lines, &exact()), Outcome::Matches);
    }

    #[test]
    fn a_snapshot_of_unknown_shape_needs_the_flag() {
        for attachment in [
            json!({"type": "prompt_snapshot"}),
            json!({"type": "prompt_snapshot", "systemPrompt": []}),
            json!({"type": "prompt_snapshot", "systemPrompt": "a bare string"}),
            json!({"type": "prompt_snapshot", "systemPrompt": ["text", {"text": "not a string"}]}),
            json!({"type": "prompt_snapshot", "systemPrompt": ["text", 7]}),
        ] {
            let lines = [json!({"type": "attachment", "attachment": attachment}).to_string()];
            let result = outcome(&lines, &exact());
            assert_eq!(result, Outcome::UnknownShape, "{attachment}");
            assert!(result.needs_snapshot_off());
        }
    }

    /// `record` with `"isSidechain": value` added, as a subagent's records carry.
    fn with_sidechain(record: &str, value: Value) -> String {
        let mut parsed: Value = serde_json::from_str(record).unwrap();
        parsed["isSidechain"] = value;
        parsed.to_string()
    }

    #[test]
    fn a_subagents_snapshot_does_not_decide_for_the_main_conversation() {
        let sidechain = |record: String| with_sidechain(&record, json!(true));
        // Newest in the file, but not the main conversation's.
        let lines = [
            snapshot(BRIEFING),
            sidechain(snapshot("a subagent's prompt")),
        ];
        assert_eq!(outcome(&lines, &exact()), Outcome::Matches);
        let lines = [snapshot("an older briefing"), sidechain(snapshot(BRIEFING))];
        assert_eq!(outcome(&lines, &exact()), Outcome::Differs);
        // A subagent's compaction is not the main conversation's either.
        let lines = [snapshot("an older briefing"), sidechain(boundary())];
        assert_eq!(outcome(&lines, &exact()), Outcome::Differs);
        let lines = [snapshot(BRIEFING), sidechain(boundary())];
        assert_eq!(outcome(&lines, &exact()), Outcome::Matches);
        // With nothing but a subagent's records there is nothing proven.
        assert_eq!(
            outcome(&[sidechain(snapshot(BRIEFING))], &exact()),
            Outcome::NoRecord
        );
    }

    #[test]
    fn only_a_literal_true_marks_a_record_as_a_subagents() {
        for value in [
            json!(false),
            json!(null),
            json!("true"),
            json!(1),
            json!([true]),
        ] {
            let lines = [
                snapshot("an older briefing"),
                with_sidechain(&snapshot(BRIEFING), value.clone()),
            ];
            assert_eq!(outcome(&lines, &exact()), Outcome::Matches, "{value}");
        }
    }

    #[test]
    fn a_boundary_must_match_on_both_type_and_subtype() {
        for record in [
            json!({"type": "system", "subtype": "compact_boundary_v2"}),
            json!({"type": "system", "subtype": "Compact_Boundary"}),
            json!({"type": "system", "subtype": " compact_boundary"}),
            json!({"type": "system", "subtype": "informational"}),
            json!({"type": "system"}),
            json!({"type": "system", "subtype": 1}),
            json!({"type": "assistant", "subtype": "compact_boundary"}),
            json!({"type": "user", "subtype": "compact_boundary"}),
            json!({"type": "attachment", "subtype": "compact_boundary"}),
            json!({"subtype": "compact_boundary"}),
            json!({"type": "systems", "subtype": "compact_boundary"}),
            // Only mentioning one, as a message that quotes it would.
            json!({"type": "user", "message": {"content": "compact_boundary"}}),
        ] {
            let lines = [snapshot("an older briefing"), record.to_string()];
            assert_eq!(outcome(&lines, &exact()), Outcome::Differs, "{record}");
        }
    }

    #[test]
    fn the_bytes_after_the_last_newline_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let text = format!(
            "{}\n{{\"type\": \"attachment\", \"attach",
            snapshot(BRIEFING)
        );
        std::fs::write(&path, text).unwrap();
        assert_eq!(decide(Some(&path), &exact()), Outcome::Matches);
    }

    #[test]
    fn a_decision_beyond_the_first_window_is_found_by_growing_it() {
        let mut lines = vec![snapshot(BRIEFING)];
        for n in 0..40 {
            lines.extend(turn(n));
        }
        let (_dir, path) = write(&lines);
        // A window far smaller than the distance to the snapshot.
        assert_eq!(
            decide_with(Some(&path), &exact(), 512, 1 << 20),
            Outcome::Matches
        );
    }

    #[test]
    fn a_decision_beyond_the_bound_needs_the_flag() {
        let mut lines = vec![snapshot(BRIEFING)];
        for n in 0..40 {
            lines.extend(turn(n));
        }
        let (_dir, path) = write(&lines);
        let result = decide_with(Some(&path), &exact(), 512, 2048);
        assert_eq!(result, Outcome::BoundReached);
        assert!(result.needs_snapshot_off());
    }

    #[test]
    fn a_single_line_longer_than_the_bound_is_not_read_as_a_record() {
        // The snapshot is complete but the window only ever holds its tail.
        let lines = [snapshot(&"x".repeat(5000))];
        let (_dir, path) = write(&lines);
        assert_eq!(
            decide_with(Some(&path), &exact(), 256, 1024),
            Outcome::BoundReached
        );
    }

    #[test]
    fn a_cut_first_line_is_never_judged() {
        // A window that begins inside the boundary line must not read it.
        let lines = [
            snapshot("an older briefing"),
            boundary(),
            other_attachment(),
        ];
        let (_dir, path) = write(&lines);
        let size = std::fs::metadata(&path).unwrap().len();
        let boundary_len = boundary().len() as u64;
        let other_len = other_attachment().len() as u64;
        // Ends inside the boundary line: the first (cut) line is skipped over.
        let window = other_len + 1 + boundary_len / 2;
        assert!(window < size);
        assert_eq!(
            decide_with(Some(&path), &exact(), window, window),
            Outcome::BoundReached
        );
    }

    #[test]
    fn byte_equality_holds_for_quotes_backslashes_japanese_and_a_trailing_newline() {
        for text in [
            "say \"hello\" and 'bye'",
            "C:\\Users\\someone\\dir and a \\n that is not a newline",
            "あなたは番頭です。班 7 の Director。",
            "ends with a newline\n",
            "ends with two\n\n",
            "tab\tand\r\nCRLF",
            "\u{1F9D1}\u{200D}\u{1F4BB} emoji",
        ] {
            let lines = [snapshot(text)];
            assert_eq!(
                outcome(&lines, &BriefingPattern::exact(text)),
                Outcome::Matches,
                "{text:?}"
            );
            // One byte of difference is a difference.
            assert_eq!(
                outcome(&lines, &BriefingPattern::exact(&format!("{text} "))),
                Outcome::Differs,
                "{text:?} + space"
            );
            assert_eq!(
                outcome(&lines, &BriefingPattern::exact(text.trim_end_matches('\n'))),
                if text.ends_with('\n') {
                    Outcome::Differs
                } else {
                    Outcome::Matches
                },
                "{text:?} trimmed"
            );
        }
    }

    fn project_with(root: &Path, project: &str, id: &str) -> PathBuf {
        let dir = root.join(project);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, "{}\n").unwrap();
        path
    }

    #[test]
    fn a_transcript_is_found_by_its_session_id_in_any_project() {
        let root = tempfile::tempdir().unwrap();
        let wanted = project_with(root.path(), "-work-alpha", "sess-1");
        project_with(root.path(), "-work-beta", "sess-2");
        assert_eq!(find_transcripts(root.path(), "sess-1"), vec![wanted]);
        assert!(find_transcripts(root.path(), "sess-3").is_empty());
    }

    #[test]
    fn every_copy_of_a_transcript_is_found() {
        let root = tempfile::tempdir().unwrap();
        let alpha = project_with(root.path(), "-work-alpha", "sess-1");
        let beta = project_with(root.path(), "-work-beta", "sess-1");
        let mut found = find_transcripts(root.path(), "sess-1");
        found.sort();
        assert_eq!(found, vec![alpha, beta]);
    }

    fn write_transcript(root: &Path, project: &str, id: &str, last_section: &str) {
        let dir = root.join(project);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            snapshot(last_section) + "\n",
        )
        .unwrap();
    }

    #[test]
    fn one_copy_that_needs_the_flag_is_enough_whichever_is_newer() {
        let pattern = BriefingPattern::exact("the briefing");
        for (alpha_first, label) in [
            (true, "matching copy written last"),
            (false, "differing last"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (first, second) = if alpha_first {
                ("an older briefing", "the briefing")
            } else {
                ("the briefing", "an older briefing")
            };
            write_transcript(root.path(), "-work-alpha", "sess-1", first);
            // Written later, so a newest-wins finder would see only this one.
            std::thread::sleep(std::time::Duration::from_millis(20));
            write_transcript(root.path(), "-work-beta", "sess-1", second);
            assert!(
                resume_needs_snapshot_off(root.path(), "sess-1", &pattern),
                "{label}"
            );
        }
    }

    #[test]
    fn no_flag_only_when_every_copy_says_so_and_a_flag_when_there_is_none() {
        let pattern = BriefingPattern::exact("the briefing");
        let root = tempfile::tempdir().unwrap();
        write_transcript(root.path(), "-work-alpha", "sess-1", "the briefing");
        assert!(!resume_needs_snapshot_off(root.path(), "sess-1", &pattern));
        write_transcript(root.path(), "-work-beta", "sess-1", "the briefing");
        assert!(!resume_needs_snapshot_off(root.path(), "sess-1", &pattern));
        // No transcript at all: nothing proves the briefing is recorded.
        assert!(resume_needs_snapshot_off(root.path(), "sess-2", &pattern));
    }

    #[test]
    fn a_session_id_that_is_not_a_plain_token_finds_nothing() {
        let root = tempfile::tempdir().unwrap();
        project_with(root.path(), "-work-alpha", "sess-1");
        std::fs::write(root.path().join("outside.jsonl"), "{}\n").unwrap();
        for id in [
            "",
            "../outside",
            "..",
            "a/b",
            "a\\b",
            "sess-1/../sess-1",
            "sess 1",
        ] {
            assert!(find_transcripts(root.path(), id).is_empty(), "{id:?}");
        }
        // A projects directory that is not there is just no transcript.
        assert!(find_transcripts(&root.path().join("absent"), "sess-1").is_empty());
        // A directory named like a transcript is not one.
        std::fs::create_dir_all(root.path().join("-work-gamma").join("sess-9.jsonl")).unwrap();
        assert!(find_transcripts(root.path(), "sess-9").is_empty());
    }

    /// A stand-in for the composer's grammar: one run with no whitespace and no
    /// comma. Enough to exercise the search; the real grammar is tested where it
    /// lives (`briefing::peers_ends`).
    fn roster(text: &str) -> Vec<usize> {
        let run = text
            .find(|c: char| c.is_whitespace() || c == ',')
            .unwrap_or(text.len());
        (1..=run)
            .filter(|&end| text.is_char_boundary(end))
            .collect()
    }

    fn with_hole(before: &str, after: &str) -> BriefingPattern {
        BriefingPattern::from_marked(&format!("{before}{PEERS_HOLE}{after}"), roster)
    }

    #[test]
    fn a_peers_hole_matches_what_the_grammar_accepts() {
        let pattern = with_hole("Peers: ", ". Be brief.");
        assert!(pattern.matches("Peers: worker-1. Be brief."));
        assert!(pattern.matches("Peers: worker-1.worker-2. Be brief."));
        let lines = [snapshot("Peers: worker-2. Be brief.")];
        assert_eq!(outcome(&lines, &pattern), Outcome::Matches);
    }

    #[test]
    fn a_peers_hole_does_not_match_what_the_grammar_refuses() {
        let pattern = with_hole("Peers: ", ". Be brief.");
        assert!(!pattern.matches("Peers: . Be brief."), "an empty hole");
        assert!(!pattern.matches("Peers: a b. Be brief."));
        assert!(!pattern.matches("Peers: a, b. Be brief."));
        assert!(!pattern.matches("Peers: a\nb. Be brief."));
    }

    #[test]
    fn text_added_or_removed_next_to_a_hole_is_a_difference() {
        // The template ends "...{peers}. They are live." and the recording was
        // made when it also said "Never merge without asking me." after the
        // roster: the roster grammar cannot stretch over a sentence.
        let pattern = with_hole("Workers: ", ". They are live.");
        assert!(pattern.matches("Workers: worker-1. They are live."));
        assert!(
            !pattern.matches("Workers: worker-1. Never merge without asking me. They are live.")
        );
        assert!(!pattern.matches("Workers: worker-1. Never merge.\n\nThey are live."));
        // The other way: the template gained a sentence the recording lacks.
        let longer = with_hole("Workers: ", ". Never merge. They are live.");
        assert!(!longer.matches("Workers: worker-1. They are live."));
        let lines = [snapshot(
            "Workers: worker-1. Never merge without asking me. They are live.",
        )];
        assert_eq!(outcome(&lines, &pattern), Outcome::Differs);
    }

    #[test]
    fn the_text_around_a_peers_hole_must_still_match() {
        let pattern = with_hole("Peers: ", ". Be brief.");
        assert!(!pattern.matches("Roster: worker-1. Be brief."));
        assert!(!pattern.matches("Peers: worker-1. Be verbose."));
        assert!(!pattern.matches("Peers: worker-1. Be brief. "));
        assert!(!pattern.matches(" Peers: worker-1. Be brief."));
        assert!(!pattern.matches("Peers: worker-1"));
        assert!(!pattern.matches(""));
        let lines = [snapshot("Peers: worker-1. Be verbose.")];
        assert_eq!(outcome(&lines, &pattern), Outcome::Differs);
    }

    #[test]
    fn the_two_ends_of_a_hole_cannot_share_bytes() {
        // "aa" + hole + "aa" needs a hole of at least a character.
        let pattern = with_hole("aa", "aa");
        assert!(pattern.matches("aaXaa"));
        assert!(
            !pattern.matches("aaaa"),
            "the grammar refuses an empty hole"
        );
        assert!(!pattern.matches("aaa"));
    }

    #[test]
    fn a_hole_whose_first_candidate_end_is_wrong_still_finds_a_later_one() {
        // The text after the hole must be exactly ".b", so the hole is "a.b",
        // not the "a" a nearest-first search tries before it.
        let pattern = BriefingPattern::from_marked(&format!("{PEERS_HOLE}.b"), roster);
        assert!(pattern.matches("a.b.b"));
        assert!(!pattern.matches("a.c"));
        let middle = BriefingPattern::from_marked(&format!("[{PEERS_HOLE}.b{PEERS_HOLE}]"), roster);
        assert!(middle.matches("[a.b.bc]"));
    }

    #[test]
    fn several_holes_match_in_order() {
        let pattern = BriefingPattern::from_marked(
            &format!("A {PEERS_HOLE} B {PEERS_HOLE} C {PEERS_HOLE} D"),
            roster,
        );
        assert!(pattern.matches("A x B y C z D"));
        assert!(!pattern.matches("A x C y B z D"), "out of order");
        assert!(!pattern.matches("A x B y D"), "a segment missing");
        assert!(!pattern.matches("A B C D"), "no empty hole");
    }

    #[test]
    fn a_template_without_a_hole_is_exact_equality() {
        let pattern = BriefingPattern::from_marked(BRIEFING, roster);
        assert!(pattern.matches(BRIEFING));
        assert!(!pattern.matches("Peers: worker-1, worker-2."));
        assert!(!pattern.matches(&format!("{BRIEFING}\n")));
        let exact = BriefingPattern::exact(BRIEFING);
        assert!(exact.matches(BRIEFING));
        assert!(!exact.matches(&format!("{BRIEFING}\n")));
    }

    #[test]
    fn a_hole_can_sit_at_either_end() {
        let at_start =
            BriefingPattern::from_marked(&format!("{PEERS_HOLE} is your roster."), roster);
        assert!(at_start.matches("worker-1 is your roster."));
        assert!(!at_start.matches("worker-1 is your roster"));
        let at_end = BriefingPattern::from_marked(&format!("Your roster: {PEERS_HOLE}"), roster);
        assert!(at_end.matches("Your roster: worker-1"));
        assert!(!at_end.matches("Our roster: worker-1"));
    }

    #[test]
    fn holes_work_across_multibyte_text() {
        let pattern = with_hole("班の仲間: ", "。以上。");
        assert!(pattern.matches("班の仲間: worker-1。以上。"));
        assert!(!pattern.matches("班の仲間: worker-1。以上"));
    }
}
