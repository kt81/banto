//! Whether a Codex member's brigade briefing still needs delivering on this
//! `SessionStart`, decided from the session's own rollout file.
//!
//! The hook (`banto _hook`) used to answer every firing with the rendered
//! briefing as `additionalContext`. Codex stores each answer as a developer
//! message in the rollout and replays all of them on resume, so a member
//! resumed twice held three identical copies (measured on a synthetic session
//! under Codex 0.169.0; real Worker rollouts held up to four copies of a
//! 4956-character text). A compaction, on the other hand, drops every earlier
//! copy, and the `SessionStart` Codex queues afterwards (source `compact`)
//! only runs at the start of the next regular turn — a process that ends
//! first never runs it. So the briefing has to be delivered whenever the live
//! history *may* lack it, and withheld only on proof that it is there.
//!
//! The proof is the rollout itself, read read-only (invariant 1 is about
//! writes): the most recent record that says anything about the briefing must
//! be a copy of this exact text. Everything else is doubt, and doubt delivers.
//!
//! - `source` other than `resume` (`startup`, `compact`, `clear`, `fork`,
//!   missing, unknown): deliver, reading nothing. A resume is the only firing
//!   whose history Codex replays whole.
//! - On `resume`, walk the rollout's complete lines backwards. A `compacted`
//!   line or a `thread_rolled_back` event means the history was rewritten
//!   after any copy behind it: deliver. A developer message holding the
//!   rendered text as one `input_text` part: suppress. A developer message
//!   with other text, and every other record, is not about this question:
//!   keep walking.
//! - Records of an unknown type are skipped on purpose. This walk is the
//!   second line behind the `source` rule; treating an unfamiliar record as
//!   doubt would switch the whole mechanism off the day Codex adds a record
//!   kind, and the copies would pile up again.
//! - Anything that cannot be read as a clean answer delivers: a missing or
//!   unreadable file, a complete line that is not JSON, a walk that reaches
//!   the start of what was read without a decision.
//!
//! **A briefing over the spill limit is never suppressed.** Codex stores a
//! hook's context longer than about 10,000 *bytes* (upstream counts
//! `ceil(bytes / 4)` approximate tokens against 2,500) as a truncated preview
//! plus a pointer to a temp file. The stored preview never equals the rendered
//! text, so such a briefing is delivered on every resume — copies accumulate
//! again for that case, and the member sees only the preview. Measured on a
//! synthetic session: 9,900 ASCII characters stored whole, 10,500 stored as a
//! preview of about 10,100 characters with the pointer. The limit is in bytes,
//! so a Japanese template of about 3,500 characters is already over it. The
//! shipped defaults are well under (a measured Worker briefing was 4,956
//! characters).
//!
//! The decision itself ([`scan_tail`]) is a pure function of bytes; only
//! [`verdict`] touches the file. It lives here, beside the other rollout
//! readers, rather than in `banto-core`: it parses an agent's file format,
//! which is I/O-side knowledge (`docs/DISCIPLINE.md` §2).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

/// First window read from the end of the rollout. Most resumes decide inside
/// it: a copy is written at the start of the session and again after each
/// compaction, and a compaction line itself ends the walk.
const START_WINDOW: u64 = 1 << 20;

/// The most of a rollout that is ever read, counted from the end. A warm
/// forward scan of a real 79 MB rollout took 0.07–0.31 s, so this stays well
/// inside what a hook that sits on a session's startup path can spend. A copy
/// that lies further back than this is not proven, so it is delivered again —
/// one extra copy, which the next compaction clears, rather than an unbounded
/// read in the operator's own startup.
const MAX_WINDOW: u64 = 32 << 20;

/// What the hook should do with the briefing it rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Return it as `additionalContext`.
    Deliver,
    /// The live history holds it already: return nothing.
    Suppress,
}

/// The decision for one `SessionStart`. `source` and `transcript_path` are the
/// hook's own stdin fields, passed on as they came (both optional upstream);
/// `briefing` is the text the hook would deliver. Never fails: every problem
/// is [`Verdict::Deliver`].
pub fn verdict(source: Option<&str>, transcript_path: Option<&Path>, briefing: &str) -> Verdict {
    verdict_with(source, transcript_path, briefing, START_WINDOW, MAX_WINDOW)
}

fn verdict_with(
    source: Option<&str>,
    transcript_path: Option<&Path>,
    briefing: &str,
    start_window: u64,
    max_window: u64,
) -> Verdict {
    if source != Some("resume") {
        return Verdict::Deliver;
    }
    let Some(path) = transcript_path else {
        return Verdict::Deliver;
    };
    match read_and_scan(path, briefing, start_window, max_window) {
        Some(Scan::Suppress) => Verdict::Suppress,
        _ => Verdict::Deliver,
    }
}

/// Reads the rollout from the end in a growing window and scans it. `None` for
/// any I/O trouble.
fn read_and_scan(path: &Path, briefing: &str, start_window: u64, max_window: u64) -> Option<Scan> {
    // Anything that is not a regular file is not a rollout, and opening a FIFO
    // for reading blocks until someone writes to it: the hook may never block.
    // The path comes from the agent's own stdin, so it is checked, not trusted.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut window = start_window;
    loop {
        let read_size = window.min(size);
        file.seek(SeekFrom::Start(size - read_size)).ok()?;
        let mut buf = vec![0u8; read_size as usize];
        file.read_exact(&mut buf).ok()?;
        match scan_tail(briefing, &buf, read_size < size) {
            Scan::Undecided if read_size < size && window < max_window => {
                window = window.saturating_mul(4).min(max_window);
            }
            other => return Some(other),
        }
    }
}

/// What a scan of one window found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    Deliver,
    Suppress,
    /// The window ran out before a decision, and more of the file lies before
    /// it: a larger window may decide.
    Undecided,
}

/// Just enough of a rollout line to route it. Deserializing into a struct
/// skips every other field without building it — a `compacted` line carries
/// the whole replacement history and can run to megabytes.
#[derive(Deserialize)]
struct Head {
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// Scans `tail` — the last bytes of a rollout, starting mid-file when
/// `starts_mid_file` — for the record that decides the question. The bytes
/// after the last `\n` are ignored: an unterminated line is the process that
/// is still writing it.
fn scan_tail(briefing: &str, tail: &[u8], starts_mid_file: bool) -> Scan {
    let undecided = if starts_mid_file {
        Scan::Undecided
    } else {
        Scan::Deliver
    };
    let Some(end) = tail.iter().rposition(|&byte| byte == b'\n') else {
        return undecided;
    };
    let lines: Vec<&[u8]> = tail[..end].split(|&byte| byte == b'\n').collect();
    for (index, line) in lines.iter().enumerate().rev() {
        if index == 0 && starts_mid_file {
            // Cut by the window, not a line of its own: nothing it says counts.
            return Scan::Undecided;
        }
        if let Some(decision) = judge_line(briefing, line) {
            return decision;
        }
    }
    undecided
}

/// `Some` when this line settles the question, `None` to keep walking.
fn judge_line(briefing: &str, line: &[u8]) -> Option<Scan> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    // A complete line that is valid JSON but not an object (`[...]`, `42`,
    // `"text"`), or whose `type` is not a string, is treated like one that does
    // not parse: it delivers. A rollout line is always an object, so anything
    // else means this is not the file the reader thinks it is, and doubt
    // delivers. (Without this check a one-element array would be read as a
    // record by the struct below.)
    if line.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Some(Scan::Deliver);
    }
    let Ok(head) = serde_json::from_slice::<Head>(line) else {
        return Some(Scan::Deliver);
    };
    match head.kind.as_deref() {
        Some("compacted") => Some(Scan::Deliver),
        Some("response_item") | Some("event_msg") => {
            let Ok(record) = serde_json::from_slice::<Value>(line) else {
                return Some(Scan::Deliver);
            };
            judge_payload(briefing, &record)
        }
        _ => None,
    }
}

fn judge_payload(briefing: &str, record: &Value) -> Option<Scan> {
    let payload = record.get("payload")?;
    let payload_type = payload.get("type").and_then(Value::as_str);
    match record.get("type").and_then(Value::as_str) {
        Some("event_msg") if payload_type == Some("thread_rolled_back") => Some(Scan::Deliver),
        Some("response_item")
            if payload_type == Some("message")
                && payload.get("role").and_then(Value::as_str) == Some("developer") =>
        {
            let parts = payload.get("content").and_then(Value::as_array)?;
            parts
                .iter()
                .any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("input_text")
                        && part.get("text").and_then(Value::as_str) == Some(briefing)
                })
                .then_some(Scan::Suppress)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::TempDir;

    const BRIEFING: &str = "You are worker-1 in brigade 7.\n\nPeers: director.";

    /// One hook copy as Codex 0.169.0 stores it (synthetic; the shape was
    /// measured, the values are made up).
    fn copy(text: &str) -> String {
        json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "ordinal": 7,
            "type": "response_item",
            "payload": {
                "type": "message",
                "id": "msg_synthetic",
                "role": "developer",
                "content": [{"type": "input_text", "text": text}],
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": "turn-synthetic",
                    "create_time": 1,
                    "content_item_kinds": ["hooks.additional_context"],
                    "content_item_metadata": [{"provenance": {"type": "harness", "hook": true}}]
                }
            },
            "metadata": {"history_sequence": 3, "sampled_through_sequence": 0, "client_authored": false}
        })
        .to_string()
    }

    fn compacted() -> String {
        json!({"type": "compacted", "payload": {"message": "", "replacement_history": []}})
            .to_string()
    }

    fn compacted_of_size(bytes: usize) -> String {
        let filler = "x".repeat(bytes);
        json!({"type": "compacted", "payload": {"replacement_history": [{"text": filler}]}})
            .to_string()
    }

    fn rolled_back() -> String {
        json!({"type": "event_msg", "payload": {"type": "thread_rolled_back", "num_turns": 1}})
            .to_string()
    }

    fn user_message() -> String {
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": BRIEFING}]}})
        .to_string()
    }

    fn unknown_record() -> String {
        json!({"type": "a_kind_codex_has_not_shipped_yet", "payload": {"type": "message"}})
            .to_string()
    }

    fn padding(count: usize) -> Vec<String> {
        (0..count)
            .map(|n| {
                json!({"type": "event_msg", "payload": {"type": "agent_message", "message": format!("{n:0>200}")}})
                    .to_string()
            })
            .collect()
    }

    /// A rollout of `lines`, newline-terminated, or — with `unterminated` —
    /// with the last line left as the writer would leave it mid-write.
    fn rollout(lines: &[String], unterminated: bool) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut text = lines.join("\n");
        if !unterminated {
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn on_resume(lines: &[String]) -> Verdict {
        let (_dir, path) = rollout(lines, false);
        verdict(Some("resume"), Some(&path), BRIEFING)
    }

    #[test]
    fn a_copy_of_the_current_text_is_suppressed() {
        assert_eq!(
            on_resume(&[copy(BRIEFING), user_message()]),
            Verdict::Suppress
        );
    }

    #[test]
    fn a_compaction_after_the_copy_delivers() {
        assert_eq!(
            on_resume(&[copy(BRIEFING), compacted(), user_message()]),
            Verdict::Deliver
        );
    }

    #[test]
    fn a_copy_after_the_last_compaction_is_suppressed() {
        assert_eq!(
            on_resume(&[compacted(), copy(BRIEFING), user_message()]),
            Verdict::Suppress
        );
        assert_eq!(
            on_resume(&[copy(BRIEFING), compacted(), copy(BRIEFING)]),
            Verdict::Suppress
        );
    }

    #[test]
    fn a_rollback_after_the_copy_delivers() {
        assert_eq!(
            on_resume(&[copy(BRIEFING), rolled_back(), user_message()]),
            Verdict::Deliver
        );
    }

    #[test]
    fn only_a_copy_of_a_different_text_delivers() {
        assert_eq!(
            on_resume(&[copy("You are worker-1 in brigade 6.")]),
            Verdict::Deliver
        );
    }

    #[test]
    fn the_current_text_behind_a_later_different_copy_is_suppressed() {
        assert_eq!(
            on_resume(&[
                copy(BRIEFING),
                copy("a briefing from some other member"),
                user_message()
            ]),
            Verdict::Suppress
        );
    }

    #[test]
    fn only_a_developer_message_counts_as_a_copy() {
        assert_eq!(on_resume(&[user_message()]), Verdict::Deliver);
    }

    #[test]
    fn the_text_must_equal_the_briefing_not_merely_contain_it() {
        assert_eq!(
            on_resume(&[copy(&format!("{BRIEFING} and more"))]),
            Verdict::Deliver
        );
        assert_eq!(
            on_resume(&[copy(BRIEFING.trim_end_matches('.'))]),
            Verdict::Deliver
        );
    }

    #[test]
    fn records_of_an_unknown_type_are_walked_past() {
        assert_eq!(
            on_resume(&[copy(BRIEFING), unknown_record(), unknown_record()]),
            Verdict::Suppress
        );
    }

    #[test]
    fn an_unterminated_last_line_is_ignored() {
        // A half-written compaction line is the live process; it decides nothing.
        let (_dir, path) = rollout(&[copy(BRIEFING), r#"{"type":"compact"#.to_string()], true);
        assert_eq!(
            verdict(Some("resume"), Some(&path), BRIEFING),
            Verdict::Suppress
        );
        // Nor does a complete-looking last line without its newline count: it
        // is not known to be whole.
        let (_dir, path) = rollout(&[user_message(), copy(BRIEFING)], true);
        assert_eq!(
            verdict(Some("resume"), Some(&path), BRIEFING),
            Verdict::Deliver
        );
    }

    #[test]
    fn a_broken_complete_line_before_any_decision_delivers() {
        assert_eq!(
            on_resume(&[
                copy(BRIEFING),
                "not json at all".to_string(),
                user_message()
            ]),
            Verdict::Deliver
        );
    }

    #[test]
    fn a_broken_line_behind_the_decision_does_not_matter() {
        assert_eq!(
            on_resume(&["not json at all".to_string(), copy(BRIEFING)]),
            Verdict::Suppress
        );
    }

    #[test]
    fn blank_lines_are_walked_past() {
        assert_eq!(
            on_resume(&[copy(BRIEFING), String::new(), "  ".to_string()]),
            Verdict::Suppress
        );
    }

    #[test]
    fn a_walk_that_reaches_the_start_without_a_decision_delivers() {
        assert_eq!(
            on_resume(&[user_message(), unknown_record()]),
            Verdict::Deliver
        );
    }

    #[test]
    fn an_empty_or_missing_file_delivers() {
        let (_dir, path) = rollout(&[], true);
        assert_eq!(
            verdict(Some("resume"), Some(&path), BRIEFING),
            Verdict::Deliver
        );
        let missing = path.with_file_name("gone.jsonl");
        assert_eq!(
            verdict(Some("resume"), Some(&missing), BRIEFING),
            Verdict::Deliver
        );
        assert_eq!(verdict(Some("resume"), None, BRIEFING), Verdict::Deliver);
        assert_eq!(
            verdict(Some("resume"), Some(Path::new("")), BRIEFING),
            Verdict::Deliver
        );
    }

    #[test]
    fn a_directory_in_place_of_the_file_delivers() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            verdict(Some("resume"), Some(dir.path()), BRIEFING),
            Verdict::Deliver
        );
    }

    #[test]
    fn every_source_but_resume_delivers_even_with_a_copy_in_the_file() {
        let (_dir, path) = rollout(&[copy(BRIEFING)], false);
        for source in [
            Some("startup"),
            Some("compact"),
            Some("clear"),
            Some("fork"),
            Some("a_source_codex_has_not_shipped_yet"),
            Some(""),
            Some("Resume"),
            None,
        ] {
            assert_eq!(
                verdict(source, Some(&path), BRIEFING),
                Verdict::Deliver,
                "source {source:?}"
            );
        }
    }

    #[test]
    fn text_with_escapes_matches_after_decoding() {
        let text = "line one\nline \"two\"\r\nback\\slash\ttab 日本語 🙂\n";
        let (_dir, path) = rollout(&[copy(text)], false);
        assert_eq!(
            verdict(Some("resume"), Some(&path), text),
            Verdict::Suppress
        );
    }

    #[test]
    fn a_very_long_compaction_line_is_handled() {
        let long = compacted_of_size(3 << 20);
        // It ends the walk when it is behind nothing…
        assert_eq!(on_resume(&[copy(BRIEFING), long.clone()]), Verdict::Deliver);
        // …and is walked past (as the boundary it is) when a copy follows it.
        assert_eq!(on_resume(&[long, copy(BRIEFING)]), Verdict::Suppress);
    }

    #[test]
    fn the_window_grows_until_a_decision_or_the_ceiling() {
        let mut lines = vec![copy(BRIEFING)];
        lines.extend(padding(60));
        let (_dir, path) = rollout(&lines, false);
        // The copy lies ~15 KB back. A window that never grows past 1 KB
        // cannot see it…
        assert_eq!(
            verdict_with(Some("resume"), Some(&path), BRIEFING, 1 << 10, 1 << 10),
            Verdict::Deliver
        );
        // …growing reaches it…
        assert_eq!(
            verdict_with(Some("resume"), Some(&path), BRIEFING, 1 << 10, 1 << 20),
            Verdict::Suppress
        );
        // …and a ceiling short of it stops there.
        assert_eq!(
            verdict_with(Some("resume"), Some(&path), BRIEFING, 1 << 10, 4 << 10),
            Verdict::Deliver
        );
    }

    #[test]
    fn crlf_line_separators_are_read_like_lf() {
        let write = |lines: &[String]| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("rollout.jsonl");
            std::fs::write(&path, lines.join("\r\n") + "\r\n").unwrap();
            (dir, path)
        };
        let (_dir, path) = write(&[copy(BRIEFING), user_message()]);
        assert_eq!(
            verdict(Some("resume"), Some(&path), BRIEFING),
            Verdict::Suppress
        );
        let (_dir, path) = write(&[copy(BRIEFING), compacted(), user_message()]);
        assert_eq!(
            verdict(Some("resume"), Some(&path), BRIEFING),
            Verdict::Deliver
        );
    }

    #[test]
    fn a_copy_after_a_rollback_is_suppressed() {
        assert_eq!(
            on_resume(&[copy(BRIEFING), rolled_back(), copy(BRIEFING)]),
            Verdict::Suppress
        );
    }

    #[test]
    fn a_window_that_starts_inside_a_multibyte_character_decides_after_it_grows() {
        // Japanese padding after the copy, so the first window's start lands in
        // the middle of a three-byte character, at every offset in turn.
        let padding: Vec<String> = (0..40)
            .map(|n| {
                json!({"type": "event_msg", "payload": {"type": "agent_message",
                    "message": format!("日本語のテキスト{n}").repeat(8)}})
                .to_string()
            })
            .collect();
        let mut lines = vec![copy(BRIEFING)];
        lines.extend(padding);
        let (_dir, path) = rollout(&lines, false);
        for start in 1000..1012 {
            assert_eq!(
                verdict_with(Some("resume"), Some(&path), BRIEFING, start, 1 << 20),
                Verdict::Suppress,
                "first window {start}"
            );
        }
        // And the same cut with nothing to find still delivers.
        let (_dir, path) = rollout(&lines[1..], false);
        for start in 1000..1012 {
            assert_eq!(
                verdict_with(Some("resume"), Some(&path), BRIEFING, start, 1 << 20),
                Verdict::Deliver,
                "first window {start}"
            );
        }
    }

    #[test]
    fn a_briefing_over_the_spill_limit_is_never_suppressed() {
        // Codex stores a context over about 10,000 bytes as a truncated preview
        // plus a pointer to a temp file; the preview is what is in the rollout,
        // so it can never equal the rendered text.
        let briefing = "日本語の指示。".repeat(1_700); // 11,900 characters, over 35,000 bytes
        assert!(briefing.len() > 10_000);
        let preview: String = briefing.chars().take(3_300).collect();
        let stored = format!("{preview}\n[… truncated; full text at /tmp/synthetic-spill.txt]");
        let (_dir, path) = rollout(&[copy(&stored), user_message()], false);
        assert_eq!(
            verdict(Some("resume"), Some(&path), &briefing),
            Verdict::Deliver
        );
        // The same kind of text under the limit does match, so the above is the
        // limit and not the multibyte characters.
        let short = "日本語の指示。".repeat(100);
        let (_dir, path) = rollout(&[copy(&short)], false);
        assert_eq!(
            verdict(Some("resume"), Some(&path), &short),
            Verdict::Suppress
        );
    }

    #[test]
    fn a_line_that_is_json_but_not_a_record_delivers_like_a_broken_one() {
        // Valid JSON that is not an object, and an object whose `type` is not a
        // string, are treated as a line that does not parse.
        for odd in [
            r#"["response_item"]"#,
            r#"["compacted"]"#,
            "42",
            r#""text""#,
            "null",
            r#"{"type":7}"#,
            r#"{"type":["compacted"]}"#,
        ] {
            assert_eq!(
                on_resume(&[copy(BRIEFING), odd.to_string(), user_message()]),
                Verdict::Deliver,
                "{odd}"
            );
        }
        // An object with no `type` at all is a record of an unknown kind and is
        // walked past, like any other.
        assert_eq!(
            on_resume(&[copy(BRIEFING), r#"{"payload":{}}"#.to_string()]),
            Verdict::Suppress
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_named_pipe_is_not_opened() {
        // Opening a FIFO for reading blocks until a writer appears; the hook
        // must never block, so only a regular file is read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let made = std::process::Command::new("mkfifo").arg(&path).status();
        if !matches!(made, Ok(status) if status.success()) {
            return;
        }
        assert_eq!(
            verdict(Some("resume"), Some(&path), BRIEFING),
            Verdict::Deliver
        );
    }

    #[test]
    fn a_line_longer_than_the_first_window_is_not_mistaken_for_a_decision() {
        // The window starts inside the long line, so what it holds of it is a
        // fragment: no verdict until the window has grown past it.
        let (_dir, path) = rollout(&[copy(BRIEFING), compacted_of_size(40 << 10)], false);
        assert_eq!(
            verdict_with(Some("resume"), Some(&path), BRIEFING, 1 << 10, 1 << 20),
            Verdict::Deliver
        );
        let (_dir, path) = rollout(&[compacted_of_size(40 << 10), copy(BRIEFING)], false);
        assert_eq!(
            verdict_with(Some("resume"), Some(&path), BRIEFING, 1 << 10, 1 << 20),
            Verdict::Suppress
        );
    }
}
