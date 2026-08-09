//! Whether a Codex brigade member's session is actively generating right
//! now, or waiting for its next instruction — the Codex-side half of the
//! relay engine's busy/idle signal (`crate::status::LiveSession`/
//! `SysinfoProbe` is the Claude-side half; Claude publishes its own
//! `status: busy` in `sessions/<pid>.json`, which Codex has no equivalent
//! of).
//!
//! # Why this is a state read, not a timing heuristic
//!
//! Two timing-based designs were tried here and both were measured, on this
//! machine, to be unsound — not just imprecise, unsound:
//!
//! 1. **Rollout file mtime.** A rollout `.jsonl`'s `LastWriteTime`, read via
//!    both `stat` and `System.IO.FileInfo`, stayed pinned at its *first*-
//!    write timestamp for an entire generation — file size grew from 18KB to
//!    75KB across roughly a minute of active writing, and the reported mtime
//!    never moved, not even after the process exited and the file closed.
//!    Whatever NTFS or the writer's I/O pattern does here, "touched in the
//!    last N seconds" reads as permanently stale from the first write
//!    onward — backwards from what a busy/idle probe needs.
//! 2. **`logs_2.sqlite` write-staleness.** An earlier version of this module
//!    called a thread idle once its `logs` rows (`crate::codex_liveness`'s
//!    module doc has the schema) went quiet for 120 seconds, a threshold
//!    picked from real gaps that were, at the time, *guessed* to be
//!    within-turn versus between-turn by raw gap size. Re-checked against
//!    the same three real sessions using the rollout's own `task_started`/
//!    `task_complete` markers (see below) as ground truth instead of a
//!    guess: the guess was wrong. The largest genuinely-within-a-turn gap
//!    measured was 135 seconds; the smallest genuinely-between-turns gap
//!    was 1 second. Those two distributions overlap completely — no fixed
//!    threshold, at any value, can separate them. A rate-limit retry or slow
//!    tool call stretching a within-turn gap past whatever threshold is
//!    chosen reads as idle and nudges a member that is still generating,
//!    which is the one failure mode this whole mechanism exists to prevent.
//!
//! What actually tracks turn state: the rollout `.jsonl` itself. Every
//! observed real turn (9, across three independent sessions) opens with an
//! `{"type":"event_msg","payload":{"type":"task_started"}}` record and
//! closes with an `{"type":"event_msg","payload":{"type":"task_complete"}}`
//! one, paired 1:1 with no exceptions. Read live against one throwaway
//! interactive session, driving two consecutive turns: the file's last such
//! marker read `task_started` throughout an active 8-second generation,
//! flipped to `task_complete` within about a second of the turn's actual
//! completion, and stayed there. This is a state read, not a clock: a
//! stalled turn — however long the stall — simply hasn't reached
//! `task_complete` yet, so it never misreads as idle no matter how long a
//! retry backoff runs. [`is_thread_idle`] reads exactly this.
//!
//! # A trap this module works around
//!
//! `task_complete`'s own record can be large — it was observed re-embedding
//! the turn's full final message, one single JSON line north of 11KB for an
//! ordinary response. A naive "read the last few KB" tail read can land
//! entirely inside that one line with no newline in the window at all,
//! finding nothing and misreporting `None` right when the answer matters
//! most (turn just finished). [`tail_turn_marker`] starts with a modest
//! window and grows it until a complete marker line is found or
//! [`TAIL_MAX_BYTES`] is hit.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::OptionalExtension;

use crate::codex_home::CodexHome;
use crate::sqlite_ro::open_read_only;

/// First tail-read window size, in bytes — comfortably larger than a bare
/// `task_started` record, small in the overwhelmingly common case where the
/// most recent turn marker is near the end of the file.
const TAIL_START_BYTES: u64 = 8 * 1024;

/// Ceiling the tail window grows to (quadrupling each retry) before
/// [`tail_turn_marker`] gives up and reports "no marker found". An ordinary
/// `task_complete` line was observed at ~11KB; this leaves roughly two
/// orders of magnitude of headroom for an unusually long single turn without
/// ever reading a whole multi-megabyte rollout just to answer one query.
const TAIL_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The two rollout markers this module distinguishes — see the module doc
/// for why they're a reliable 1:1 pair rather than a name banto invented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnMarker {
    TaskStarted,
    TaskComplete,
}

/// Where [`is_thread_idle`]'s own cost actually goes — the sqlite lookup
/// ([`rollout_path_for`]: stat, open, one `SELECT`) and the rollout tail
/// read ([`tail_turn_marker`]: open plus a growing 8KiB-to-4MiB read) have
/// nothing in common and completely different fixes if either turns out
/// slow, so a caller bracketing the whole call from outside cannot tell
/// which one to chase. Measured unconditionally, same discretion as
/// `banto::embedded::emporium`'s own `StageTimings`: durations only.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexActivityTiming {
    pub sqlite: Duration,
    pub rollout: Duration,
}

/// Whether `session_id`'s Codex session looks idle right now, read from its
/// rollout file's most recent turn marker. `None` when nothing can be
/// determined: `state_5.sqlite` missing/unopenable, no `threads` row for
/// this id, the rollout file missing/unopenable, or a rollout with no turn
/// marker in it at all (e.g. a brand-new session that hasn't sent a first
/// message yet). Never guesses toward "idle" on missing data — see this
/// module's doc for why a false idle is the one outcome to avoid.
///
/// `timing`, when given, is filled in regardless of the answer — an
/// out-parameter rather than a second return value, deliberately, so the
/// four call sites (three tests, one real) that do not care about it stay
/// exactly as clean as the result alone would leave them, and so this
/// matches the shape `banto::embedded::emporium`'s own
/// `gather_fork_observations`/`gather_relay_observations`/
/// `gather_goinkyo_observation` already use for the identical concern:
/// `Option<&mut Timings>`, accumulated into by the callee, not returned —
/// one shape for "does anyone want to know how long this took" across the
/// whole change, rather than a different one just because this function
/// happens to sit in a different crate.
pub fn is_thread_idle(
    codex_home: &CodexHome,
    session_id: &str,
    mut timing: Option<&mut CodexActivityTiming>,
) -> Option<bool> {
    let sqlite_start = Instant::now();
    let rollout_path = rollout_path_for(codex_home, session_id);
    if let Some(t) = timing.as_deref_mut() {
        t.sqlite = sqlite_start.elapsed();
    }
    let rollout_path = rollout_path?;

    let rollout_start = Instant::now();
    let marker = tail_turn_marker(&rollout_path);
    if let Some(t) = timing {
        t.rollout = rollout_start.elapsed();
    }
    let marker = marker?;

    Some(marker == TurnMarker::TaskComplete)
}

/// Looks up `<codex_home>/state_5.sqlite`'s `threads.rollout_path` for
/// `session_id` — read-only, through [`open_read_only`]'s safe-against-a-
/// concurrent-writer strategy, the same one [`crate::provider::codex`] and
/// [`crate::codex_liveness`] already use against this product's databases.
fn rollout_path_for(codex_home: &CodexHome, session_id: &str) -> Option<PathBuf> {
    let db_path = codex_home.threads_db_path();
    if !db_path.exists() {
        return None;
    }
    let conn = open_read_only(&db_path).ok()?;
    let path: Option<String> = conn
        .query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    path.map(PathBuf::from)
}

/// Reads `path` from the end, in a growing window (see [`TAIL_MAX_BYTES`]),
/// and returns the most recent [`TurnMarker`] found — a non-marker record
/// after it (e.g. a `thread_settings_applied` sync event observed trailing
/// a real `task_complete`) does not shadow it, since this scans backward
/// for the marker specifically rather than trusting the literal last line.
fn tail_turn_marker(path: &Path) -> Option<TurnMarker> {
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut window = TAIL_START_BYTES;
    loop {
        let read_size = window.min(size);
        file.seek(SeekFrom::Start(size - read_size)).ok()?;
        let mut buf = vec![0u8; read_size as usize];
        file.read_exact(&mut buf).ok()?;
        if let Some(marker) = last_turn_marker_in(&buf, read_size < size) {
            return Some(marker);
        }
        if read_size >= size || window >= TAIL_MAX_BYTES {
            return None;
        }
        window = (window * 4).min(TAIL_MAX_BYTES);
    }
}

/// Scans `buf` (a tail slice of a rollout file) line by line, from the end,
/// for the most recent [`TurnMarker`]. `leading_may_be_partial` drops the
/// first line when `buf` doesn't start at byte 0 of the file — a seek into
/// the middle of a line otherwise reads as a corrupt one, which
/// [`parse_turn_marker`] already tolerates, but dropping it avoids relying
/// on that tolerance for something that isn't actually malformed.
fn last_turn_marker_in(buf: &[u8], leading_may_be_partial: bool) -> Option<TurnMarker> {
    let text = String::from_utf8_lossy(buf);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if leading_may_be_partial && !lines.is_empty() {
        lines.remove(0);
    }
    lines
        .into_iter()
        .rev()
        .find_map(|line| parse_turn_marker(line.trim()))
}

/// Parses one rollout JSONL line, returning `Some` only for
/// `{"type":"event_msg","payload":{"type":"task_started"|"task_complete"}}`.
/// Every other record shape (malformed JSON, a different `type`, a
/// different `event_msg` payload) is `None` — leniently, the same contract
/// as every other rollout/log reader in this crate.
fn parse_turn_marker(line: &str) -> Option<TurnMarker> {
    if line.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    match value.get("payload")?.get("type")?.as_str()? {
        "task_started" => Some(TurnMarker::TaskStarted),
        "task_complete" => Some(TurnMarker::TaskComplete),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn write_rollout(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    fn task_started() -> String {
        r#"{"type":"event_msg","payload":{"type":"task_started"}}"#.to_string()
    }

    fn task_complete() -> String {
        r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#.to_string()
    }

    fn user_message() -> String {
        r#"{"type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#.to_string()
    }

    // -- tail_turn_marker ---------------------------------------------

    #[test]
    fn a_rollout_ending_in_task_started_is_mid_turn() {
        let dir = TempDir::new().unwrap();
        let path = write_rollout(dir.path(), "r.jsonl", &[task_started(), user_message()]);
        assert_eq!(tail_turn_marker(&path), Some(TurnMarker::TaskStarted));
    }

    #[test]
    fn a_rollout_ending_in_task_complete_is_idle() {
        let dir = TempDir::new().unwrap();
        let path = write_rollout(
            dir.path(),
            "r.jsonl",
            &[task_started(), user_message(), task_complete()],
        );
        assert_eq!(tail_turn_marker(&path), Some(TurnMarker::TaskComplete));
    }

    #[test]
    fn a_second_turn_flips_back_to_task_started() {
        let dir = TempDir::new().unwrap();
        let path = write_rollout(
            dir.path(),
            "r.jsonl",
            &[
                task_started(),
                task_complete(),
                task_started(),
                user_message(),
            ],
        );
        assert_eq!(tail_turn_marker(&path), Some(TurnMarker::TaskStarted));
    }

    #[test]
    fn trailing_non_turn_events_after_task_complete_do_not_hide_it() {
        // Observed on a real idle session: a settings-sync event_msg trails
        // the real task_complete. The literal last line is not the marker,
        // but the backward scan must still find it.
        let dir = TempDir::new().unwrap();
        let settings_synced =
            r#"{"type":"event_msg","payload":{"type":"thread_settings_applied"}}"#.to_string();
        let path = write_rollout(
            dir.path(),
            "r.jsonl",
            &[
                task_started(),
                task_complete(),
                settings_synced.clone(),
                settings_synced,
            ],
        );
        assert_eq!(tail_turn_marker(&path), Some(TurnMarker::TaskComplete));
    }

    #[test]
    fn an_oversized_task_complete_line_is_still_found_by_growing_the_window() {
        // Reproduces the exact bug this module's growing-window read exists
        // to avoid: task_complete's own line can re-embed the full final
        // message and run well past a small fixed tail window. A single
        // fixed 8KB read that happens to land inside this line (no newline
        // in the window at all) would report no marker found.
        let dir = TempDir::new().unwrap();
        let huge_payload = "x".repeat(20 * 1024); // well past TAIL_START_BYTES
        let huge_task_complete = format!(
            r#"{{"type":"event_msg","payload":{{"type":"task_complete","message":"{huge_payload}"}}}}"#
        );
        let path = write_rollout(
            dir.path(),
            "r.jsonl",
            &[task_started(), user_message(), huge_task_complete],
        );
        assert_eq!(tail_turn_marker(&path), Some(TurnMarker::TaskComplete));
    }

    #[test]
    fn a_rollout_with_no_turn_marker_at_all_is_none() {
        let dir = TempDir::new().unwrap();
        let path = write_rollout(dir.path(), "r.jsonl", &[user_message()]);
        assert_eq!(tail_turn_marker(&path), None);
    }

    #[test]
    fn a_missing_rollout_file_is_none() {
        let dir = TempDir::new().unwrap();
        assert_eq!(tail_turn_marker(&dir.path().join("nope.jsonl")), None);
    }

    #[test]
    fn an_empty_rollout_file_is_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("r.jsonl");
        std::fs::write(&path, "").unwrap();
        assert_eq!(tail_turn_marker(&path), None);
    }

    #[test]
    fn a_malformed_trailing_line_falls_back_to_the_last_good_marker() {
        let dir = TempDir::new().unwrap();
        let path = write_rollout(
            dir.path(),
            "r.jsonl",
            &[task_started(), "{not valid json".to_string()],
        );
        assert_eq!(tail_turn_marker(&path), Some(TurnMarker::TaskStarted));
    }

    // -- rollout_path_for -----------------------------------------------

    fn threads_db(dir: &TempDir) -> CodexHome {
        let home = CodexHome::new(dir.path().to_path_buf());
        std::fs::create_dir_all(home.root()).unwrap();
        let conn = Connection::open(home.threads_db_path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL)",
        )
        .unwrap();
        home
    }

    #[test]
    fn rollout_path_for_reads_the_matching_row() {
        let dir = TempDir::new().unwrap();
        let home = threads_db(&dir);
        Connection::open(home.threads_db_path())
            .unwrap()
            .execute(
                "INSERT INTO threads (id, rollout_path) VALUES ('t1', 'C:/rollouts/t1.jsonl')",
                [],
            )
            .unwrap();
        assert_eq!(
            rollout_path_for(&home, "t1"),
            Some(PathBuf::from("C:/rollouts/t1.jsonl"))
        );
    }

    #[test]
    fn rollout_path_for_is_none_for_an_unknown_id() {
        let dir = TempDir::new().unwrap();
        let home = threads_db(&dir);
        assert_eq!(rollout_path_for(&home, "nope"), None);
    }

    #[test]
    fn rollout_path_for_is_none_when_state_db_is_missing() {
        let dir = TempDir::new().unwrap();
        let home = CodexHome::new(dir.path().to_path_buf());
        assert_eq!(rollout_path_for(&home, "t1"), None);
    }

    // -- is_thread_idle: the full path ------------------------------------

    #[test]
    fn is_thread_idle_end_to_end_mid_turn_and_idle() {
        let dir = TempDir::new().unwrap();
        let home = threads_db(&dir);
        let rollout = write_rollout(dir.path(), "r.jsonl", &[task_started(), user_message()]);
        Connection::open(home.threads_db_path())
            .unwrap()
            .execute(
                "INSERT INTO threads (id, rollout_path) VALUES ('t1', ?1)",
                [rollout.to_string_lossy().to_string()],
            )
            .unwrap();
        assert_eq!(is_thread_idle(&home, "t1", None), Some(false));

        std::fs::write(
            &rollout,
            format!(
                "{}\n{}\n{}\n",
                task_started(),
                user_message(),
                task_complete()
            ),
        )
        .unwrap();
        assert_eq!(is_thread_idle(&home, "t1", None), Some(true));
    }

    #[test]
    fn is_thread_idle_is_none_for_an_unknown_session() {
        let dir = TempDir::new().unwrap();
        let home = threads_db(&dir);
        assert_eq!(is_thread_idle(&home, "nope", None), None);
    }

    #[test]
    fn is_thread_idle_reports_sqlite_time_but_no_rollout_time_when_the_thread_is_unknown() {
        // The rollout lookup never runs at all when the sqlite step already
        // came up empty — `rollout` should read as the zero default, not
        // some incidental non-zero value. Not asserting `sqlite` is
        // non-zero: a real read can land at 0us on a fast machine with a
        // warm page cache, and this test must not become flaky by
        // demanding otherwise — the shape this proves is that `rollout`
        // specifically is skipped, not that either duration is large.
        let dir = TempDir::new().unwrap();
        let home = threads_db(&dir);
        let mut timing = CodexActivityTiming::default();
        is_thread_idle(&home, "nope", Some(&mut timing));
        assert_eq!(timing.rollout, Duration::ZERO);
    }
}
