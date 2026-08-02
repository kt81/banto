//! Whether a Codex brigade member's session is actively generating right
//! now, or waiting for its next instruction — the Codex-side half of the
//! relay engine's busy/idle signal (`crate::status::LiveSession`/
//! `SysinfoProbe` is the Claude-side half; Claude publishes its own
//! `status: busy` in `sessions/<pid>.json`, which Codex has no equivalent
//! of).
//!
//! # What this reads, and why not the rollout file
//!
//! The obvious first guess — "the rollout `.jsonl` file's mtime hasn't
//! moved recently" — was measured against a real Codex process on this
//! machine (Windows) and rejected: a rollout file's `LastWriteTime`, read
//! both via `stat` and via `System.IO.FileInfo`, stayed pinned at its
//! *first*-write timestamp for the entire run — file size grew from 18KB to
//! 75KB across roughly a minute of active generation, and the reported
//! mtime never moved off its initial value, not even after the process
//! exited and the file closed. Whatever NTFS or the writer's own I/O
//! pattern is doing here, "has this file been touched in the last N
//! seconds" is not a real-time signal for it on this platform — it would
//! read as permanently stale from the first write onward, which is exactly
//! backwards from what a busy/idle probe needs.
//!
//! What does move in real time: `<codex_home>/logs_2.sqlite`'s `logs` table
//! (`crate::codex_liveness`'s module doc has the schema), which Codex
//! stamps with `thread_id` on rows belonging to a specific session's
//! traffic. Read (not synthesized) against three real, independently
//! long-lived interactive sessions on this machine: rows tagged
//! `codex_api::sse::responses` / `codex_core::stream_events_utils` land
//! every 0-2 seconds while a turn is actively streaming, with occasional
//! gaps up to 77 seconds observed mid-turn (a slow tool call or API
//! latency); once a turn finishes and the session is genuinely waiting on
//! its next instruction, logging for that `thread_id` goes completely
//! silent — no heartbeat, no periodic noise — for anywhere from minutes to
//! multiple days, with the shortest such observed gap being 135 seconds.
//! [`CODEX_IDLE_QUIET_PERIOD`] sits between those two measured bounds.
//!
//! # The residual risk this still carries
//!
//! A tool call whose own execution logs nothing for longer than
//! [`CODEX_IDLE_QUIET_PERIOD`] (a slow build, a long-running test suite)
//! would read as idle mid-turn — none of the sampled real sessions showed
//! this, but the sample is three sessions on one machine, not a proof it
//! cannot happen. Accepted deliberately: the failure mode on a false
//! "idle" here is a nudge's keystrokes landing in a pane that is still
//! working, no worse than the general class of risk the relay engine
//! already carries, and raising the threshold further only slows down
//! every *correctly* detected idle window too.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::codex_home::CodexHome;
use crate::sqlite_ro::open_read_only;

/// How long `logs_2.sqlite` must show zero rows for a `thread_id` before
/// [`is_thread_idle`] calls it idle — see the module doc's measured
/// bounds (max observed mid-turn gap 77s, min observed between-turns gap
/// 135s). Deliberately the higher end of that band: a late nudge costs
/// nothing but a few seconds' delay, an early one risks landing keystrokes
/// mid-generation.
pub const CODEX_IDLE_QUIET_PERIOD: Duration = Duration::from_secs(120);

/// Whether `thread_id`'s Codex session looks idle as of `now` — no
/// `logs_2.sqlite` activity for that thread in the last
/// [`CODEX_IDLE_QUIET_PERIOD`]. `now` is taken as a parameter rather than
/// read internally so this stays trivially testable against a fixed clock.
///
/// `None` when nothing can be determined: a missing `logs_2.sqlite`, an
/// unopenable one, or — critically — a `thread_id` with no rows logged for
/// it at all (a session too new to have logged anything yet reads the same
/// as one banto simply doesn't recognize; both must default to "unknown",
/// never to "idle", so a session that hasn't had the chance to prove
/// itself busy is never nudged on the strength of silence alone).
pub fn is_thread_idle(codex_home: &CodexHome, thread_id: &str, now: SystemTime) -> Option<bool> {
    let db_path = codex_home.logs_db_path();
    if !db_path.exists() {
        return None;
    }
    let conn = open_read_only(&db_path).ok()?;
    let newest_ts: Option<i64> = conn
        .query_row(
            "SELECT MAX(ts) FROM logs WHERE thread_id = ?1",
            [thread_id],
            |row| row.get(0),
        )
        .ok()?;
    let newest_ts = newest_ts?;
    let now_unix = now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    Some(now_unix.saturating_sub(newest_ts) >= CODEX_IDLE_QUIET_PERIOD.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    const CREATE_LOGS: &str = "\
        CREATE TABLE logs (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            ts INTEGER NOT NULL, \
            ts_nanos INTEGER NOT NULL, \
            level TEXT NOT NULL, \
            target TEXT NOT NULL, \
            thread_id TEXT, \
            process_uuid TEXT\
        )";

    fn codex_home(dir: &TempDir) -> CodexHome {
        CodexHome::new(dir.path().to_path_buf())
    }

    fn write_log_row(home: &CodexHome, ts: i64, thread_id: &str) {
        std::fs::create_dir_all(home.root()).unwrap();
        let conn = Connection::open(home.logs_db_path()).unwrap();
        conn.execute_batch(CREATE_LOGS).ok(); // no-op once the table exists
        conn.execute(
            "INSERT INTO logs (ts, ts_nanos, level, target, thread_id, process_uuid) \
             VALUES (?1, 0, 'INFO', 'codex_core', ?2, NULL)",
            rusqlite::params![ts, thread_id],
        )
        .unwrap();
    }

    fn at(unix_secs: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(unix_secs as u64)
    }

    #[test]
    fn recent_activity_is_not_idle() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "thread-1");
        // 10s after the last row: well under CODEX_IDLE_QUIET_PERIOD.
        assert_eq!(is_thread_idle(&home, "thread-1", at(1_010)), Some(false));
    }

    #[test]
    fn activity_right_at_the_threshold_is_idle() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "thread-1");
        let boundary = 1_000 + CODEX_IDLE_QUIET_PERIOD.as_secs() as i64;
        assert_eq!(is_thread_idle(&home, "thread-1", at(boundary)), Some(true));
        assert_eq!(
            is_thread_idle(&home, "thread-1", at(boundary - 1)),
            Some(false)
        );
    }

    #[test]
    fn the_newest_row_for_the_thread_is_what_counts() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "thread-1");
        write_log_row(&home, 5_000, "thread-1");
        assert_eq!(is_thread_idle(&home, "thread-1", at(5_010)), Some(false));
    }

    #[test]
    fn a_different_threads_activity_does_not_count() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 5_000, "someone-elses-thread");
        // No row at all for "thread-1": unknown, not idle-by-default.
        assert_eq!(is_thread_idle(&home, "thread-1", at(5_010)), None);
    }

    #[test]
    fn a_thread_with_no_rows_at_all_is_unknown() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        std::fs::create_dir_all(home.root()).unwrap();
        Connection::open(home.logs_db_path())
            .unwrap()
            .execute_batch(CREATE_LOGS)
            .unwrap();
        assert_eq!(is_thread_idle(&home, "thread-1", at(1_000)), None);
    }

    #[test]
    fn a_missing_logs_database_is_unknown() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        assert_eq!(is_thread_idle(&home, "thread-1", at(1_000)), None);
    }
}
