//! Codex session liveness — the Codex-side half of CLAUDE.md invariant 4
//! (never double-resume). Separate from [`crate::status`], which stays
//! product-agnostic (bare pid arithmetic, no process-name matching) and is
//! not touched by this module at all: the per-product dispatch this would
//! otherwise force into `status/` lives here, and in this module's own
//! callers, instead.
//!
//! Codex has no per-session live-state file the way Claude's
//! `sessions/<pid>.json` is one. What it has: `logs_2.sqlite`'s `logs`
//! table —
//! ```sql
//! CREATE TABLE logs (
//!     id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
//!     ts_nanos INTEGER NOT NULL, level TEXT NOT NULL, target TEXT NOT NULL,
//!     thread_id TEXT, process_uuid TEXT, ...
//! );
//! CREATE INDEX idx_logs_thread_id_ts ON logs(thread_id, ts DESC, ts_nanos DESC, id DESC);
//! ```
//! (real schema, read from a real `~/.codex/logs_2.sqlite` on this machine,
//! 2026-07-27 — not assumed). `process_uuid` is `pid:<PID>:<suffix>`; the
//! newest row per `thread_id` (the index above exists for exactly this
//! query) names the pid that most recently wrote to that session. Confirmed
//! against that same real, live database: the newest row's pid for the
//! session actually running that moment matched a real, live process
//! (cross-checked via `Win32_Process`), and three finished sessions'
//! newest rows named three dead pids.

use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::codex_home::CodexHome;
use crate::sqlite_ro::open_read_only;

/// A process's own start time, for the pid-recycling guard
/// [`is_thread_alive`] needs. Its own small trait, not a `status::
/// ProcessProbe` method: `status/` stays free of any Codex-shaped concern,
/// full stop — see this module's own doc.
pub trait ProcessStartTime {
    /// `None` if no process is alive at `pid` right now. `Some(unix_seconds)`
    /// — that process's own start time — otherwise. Both facts come from
    /// one query so there is no gap between "is it alive" and "when did it
    /// start" for a pid to change identity in.
    fn start_time(&self, pid: u32) -> Option<u64>;
}

/// [`ProcessStartTime`] backed by the `sysinfo` crate — the same query
/// pattern as `status::probe::SysinfoProbe::is_alive` (refresh only the
/// queried pid, cheapest refresh kind, never walk the full process table),
/// duplicated rather than reused: that type lives in `status/`, which this
/// module does not depend on.
///
/// Deliberately *does* go through `sysinfo`'s own `Process::start_time()`
/// here — unlike `SysinfoProbe::is_alive_matching`, which avoids it because
/// that check needs Claude's raw `/proc` tick count for an exact match.
/// This check only needs a wall-clock comparison against `logs.ts` (unix
/// seconds — confirmed against a real log row's value next to the current
/// time), which is exactly what `start_time()` reports, on every platform
/// `sysinfo` supports — including Windows, where the Claude-side ticks
/// comparison has no equivalent at all.
#[derive(Debug, Default, Clone, Copy)]
pub struct SysinfoStartTime;

impl ProcessStartTime for SysinfoStartTime {
    fn start_time(&self, pid: u32) -> Option<u64> {
        let pid = Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing(),
        );
        system.process(pid).map(|p| p.start_time())
    }
}

/// Whether `thread_id`'s Codex session is currently alive.
///
/// Degrades to `false` — never an error, never a panic — for every failure
/// mode: a missing `logs_2.sqlite` (Codex never run, or logging off), a
/// `thread_id` with no rows yet (the startup window before anything is
/// tagged), a malformed `process_uuid`, or a pruned/corrupt log database.
/// `false` is the safe direction here: a caller uses this to *refuse* a
/// resume (CLAUDE.md invariant 4) only when it gets back a positive "yes,
/// this is live" signal, so under-reporting liveness never manufactures a
/// refusal banto can't justify — the same asymmetry `banto_io::status`'s
/// own bare-pid fallback already accepts on the Claude side. The residual
/// risk this leaves — a session live in that startup window still gets
/// resumed — is the same class of risk Claude's own live-file check
/// already carries for a session that hasn't written `sessions/<pid>.json`
/// yet; both are secondary guards behind the pane-map's own primary one,
/// not the only line of defense.
pub fn is_thread_alive(
    codex_home: &CodexHome,
    thread_id: &str,
    start_time: &dyn ProcessStartTime,
) -> bool {
    let db_path = codex_home.logs_db_path();
    if !db_path.exists() {
        return false;
    }
    let Ok(conn) = open_read_only(&db_path) else {
        return false;
    };
    newest_thread_is_alive(&conn, thread_id, start_time)
}

/// Every thread for which Codex's log database can currently prove a live
/// process.  Opens the database once for a pull that needs to inspect many
/// sessions, rather than repeating the connection setup per row.
pub fn live_thread_ids(
    codex_home: &CodexHome,
    start_time: &dyn ProcessStartTime,
) -> BTreeSet<String> {
    let db_path = codex_home.logs_db_path();
    if !db_path.exists() {
        return BTreeSet::new();
    }
    let Ok(conn) = open_read_only(&db_path) else {
        return BTreeSet::new();
    };
    let Ok(mut statement) = conn.prepare(
        "SELECT DISTINCT thread_id FROM logs WHERE thread_id IS NOT NULL AND process_uuid IS NOT NULL",
    ) else {
        return BTreeSet::new();
    };
    let Ok(ids) = statement.query_map([], |row| row.get::<_, String>(0)) else {
        return BTreeSet::new();
    };
    ids.flatten()
        .filter(|thread_id| newest_thread_is_alive(&conn, thread_id, start_time))
        .collect()
}

/// Return the subset of `candidate_ids` whose newest Codex log row still
/// proves a live process.  This is the resident-list counterpart to
/// [`live_thread_ids`]: it deliberately limits SQLite work to the rows the
/// caller is about to render, then refreshes every candidate PID through one
/// `sysinfo::System` snapshot rather than constructing one system per PID.
///
/// Missing/corrupt data degrades to an empty set.  That is an honest absence
/// of proof, never a claim that a Codex session is dead.
pub fn live_candidate_thread_ids(
    codex_home: &CodexHome,
    candidate_ids: impl IntoIterator<Item = String>,
) -> BTreeSet<String> {
    let candidate_ids = candidate_ids.into_iter().collect::<Vec<_>>();
    if candidate_ids.is_empty() {
        return BTreeSet::new();
    }
    let db_path = codex_home.logs_db_path();
    if !db_path.exists() {
        return BTreeSet::new();
    }
    let Ok(conn) = open_read_only(&db_path) else {
        return BTreeSet::new();
    };
    let candidates = newest_candidate_processes(&conn, &candidate_ids);
    if candidates.is_empty() {
        return BTreeSet::new();
    }
    let pids = candidates
        .iter()
        .map(|candidate| Pid::from_u32(candidate.pid))
        .collect::<Vec<_>>();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing(),
    );
    candidates
        .into_iter()
        .filter(|candidate| {
            system
                .process(Pid::from_u32(candidate.pid))
                .is_some_and(|process| process.start_time() as i64 <= candidate.log_ts)
        })
        .map(|candidate| candidate.thread_id)
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct CandidateProcess {
    thread_id: String,
    log_ts: i64,
    pid: u32,
}

/// Read one newest process record per already-known candidate.  The resident
/// caller supplies the list from `threads`, so this never scans every thread
/// that has ever appeared in `logs` (unlike pull-only [`live_thread_ids`]).
fn newest_candidate_processes(
    conn: &Connection,
    candidate_ids: &[String],
) -> Vec<CandidateProcess> {
    let Ok(mut statement) = conn.prepare(
        "SELECT ts, process_uuid FROM logs \
         WHERE thread_id = ?1 AND process_uuid IS NOT NULL \
         ORDER BY ts DESC, ts_nanos DESC, id DESC LIMIT 1",
    ) else {
        return Vec::new();
    };
    candidate_ids
        .iter()
        .filter_map(|thread_id| {
            statement
                .query_row([thread_id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .optional()
                .ok()
                .flatten()
                .and_then(|(log_ts, process_uuid)| {
                    parse_pid(&process_uuid).map(|pid| CandidateProcess {
                        thread_id: thread_id.clone(),
                        log_ts,
                        pid,
                    })
                })
        })
        .collect()
}

fn newest_thread_is_alive(
    conn: &Connection,
    thread_id: &str,
    start_time: &dyn ProcessStartTime,
) -> bool {
    let newest: Option<(i64, String)> = conn
        .query_row(
            "SELECT ts, process_uuid FROM logs \
             WHERE thread_id = ?1 AND process_uuid IS NOT NULL \
             ORDER BY ts DESC, ts_nanos DESC, id DESC LIMIT 1",
            [thread_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .ok()
        .flatten();
    let Some((log_ts, process_uuid)) = newest else {
        return false;
    };
    let Some(pid) = parse_pid(&process_uuid) else {
        return false;
    };
    let Some(started_at) = start_time.start_time(pid) else {
        return false;
    };
    // Recycling guard: a process that started strictly after this log row
    // was written cannot be the one that wrote it, even though it now
    // holds the same pid.
    started_at as i64 <= log_ts
}

/// Parse the pid out of a `process_uuid` column value (`pid:<PID>:<suffix>`).
fn parse_pid(process_uuid: &str) -> Option<u32> {
    process_uuid
        .strip_prefix("pid:")?
        .split(':')
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::collections::HashMap;
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

    fn write_log_row(home: &CodexHome, ts: i64, thread_id: &str, process_uuid: Option<&str>) {
        std::fs::create_dir_all(home.root()).unwrap();
        let conn = Connection::open(home.logs_db_path()).unwrap();
        conn.execute_batch(CREATE_LOGS).ok(); // no-op once the table exists
        conn.execute(
            "INSERT INTO logs (ts, ts_nanos, level, target, thread_id, process_uuid) \
             VALUES (?1, 0, 'INFO', 'codex_core', ?2, ?3)",
            rusqlite::params![ts, thread_id, process_uuid],
        )
        .unwrap();
    }

    /// A [`ProcessStartTime`] backed by a fixed map, for deterministic tests.
    struct MockStartTime(HashMap<u32, u64>);

    impl ProcessStartTime for MockStartTime {
        fn start_time(&self, pid: u32) -> Option<u64> {
            self.0.get(&pid).copied()
        }
    }
    #[test]
    fn parse_pid_extracts_the_middle_field() {
        assert_eq!(parse_pid("pid:44220:6596caaf-a68f"), Some(44220));
    }
    #[test]
    fn parse_pid_rejects_a_non_numeric_or_malformed_value() {
        assert_eq!(parse_pid("pid:not-a-number:x"), None);
        assert_eq!(parse_pid("not-even-close"), None);
        assert_eq!(parse_pid(""), None);
    }

    #[test]
    fn missing_logs_db_is_not_alive() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let start_time = MockStartTime(HashMap::new());
        assert!(!is_thread_alive(&home, "thread-1", &start_time));
    }

    #[test]
    fn a_thread_with_no_rows_yet_is_not_alive() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "other-thread", Some("pid:100:x"));
        let start_time = MockStartTime(HashMap::from([(100, 500)]));
        assert!(!is_thread_alive(&home, "thread-1", &start_time));
    }

    #[test]
    fn a_dead_pid_is_not_alive() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "thread-1", Some("pid:100:x"));
        // No entry for pid 100: MockStartTime reports it as not running.
        let start_time = MockStartTime(HashMap::new());
        assert!(!is_thread_alive(&home, "thread-1", &start_time));
    }

    #[test]
    fn a_live_pid_that_started_before_the_log_row_is_alive() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "thread-1", Some("pid:100:x"));
        let start_time = MockStartTime(HashMap::from([(100, 900)]));
        assert!(is_thread_alive(&home, "thread-1", &start_time));
    }

    #[test]
    fn a_recycled_pid_that_started_after_the_log_row_is_not_alive() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "thread-1", Some("pid:100:x"));
        // A different, unrelated process now holds pid 100, started well
        // after this log row was written — not the process that wrote it.
        let start_time = MockStartTime(HashMap::from([(100, 5_000)]));
        assert!(!is_thread_alive(&home, "thread-1", &start_time));
    }

    #[test]
    fn the_newest_row_for_a_thread_wins_over_an_older_dead_one() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "thread-1", Some("pid:100:x")); // older, dead
        write_log_row(&home, 2_000, "thread-1", Some("pid:200:x")); // newer, alive
        let start_time = MockStartTime(HashMap::from([(200, 1_500)]));
        assert!(is_thread_alive(&home, "thread-1", &start_time));
    }

    #[test]
    fn a_row_with_no_process_uuid_is_skipped_in_favor_of_an_older_one_that_has_one() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "thread-1", Some("pid:100:x"));
        write_log_row(&home, 2_000, "thread-1", None); // newest, but no pid info
        let start_time = MockStartTime(HashMap::from([(100, 500)]));
        assert!(is_thread_alive(&home, "thread-1", &start_time));
    }

    #[test]
    fn live_thread_ids_collects_only_threads_with_a_live_current_process() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "live", Some("pid:100:x"));
        write_log_row(&home, 1_000, "dead", Some("pid:200:x"));
        let start_time = MockStartTime(HashMap::from([(100, 900)]));
        assert_eq!(
            live_thread_ids(&home, &start_time),
            std::collections::BTreeSet::from(["live".to_string()])
        );
    }

    #[test]
    fn candidate_processes_reads_only_requested_newest_rows() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "wanted", Some("pid:100:old"));
        write_log_row(&home, 2_000, "wanted", Some("pid:200:new"));
        write_log_row(&home, 3_000, "unwanted", Some("pid:300:x"));
        let conn = open_read_only(&home.logs_db_path()).unwrap();
        assert_eq!(
            newest_candidate_processes(&conn, &["wanted".to_string(), "missing".to_string()]),
            vec![CandidateProcess {
                thread_id: "wanted".to_string(),
                log_ts: 2_000,
                pid: 200,
            }]
        );
    }

    #[test]
    fn a_malformed_process_uuid_is_not_alive() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        write_log_row(&home, 1_000, "thread-1", Some("garbage"));
        let start_time = MockStartTime(HashMap::from([(100, 500)]));
        assert!(!is_thread_alive(&home, "thread-1", &start_time));
    }

    #[test]
    fn discover_reads_a_warm_database_with_an_active_writer() {
        // The connection is deliberately kept open (not dropped): -wal and
        // -shm stay present, so this exercises the mode=ro open path
        // against a database an active writer still holds — the same
        // independent re-check P2a ran against state_5.sqlite, this time
        // against logs_2.sqlite's own schema.
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        std::fs::create_dir_all(home.root()).unwrap();
        let conn = Connection::open(home.logs_db_path()).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(CREATE_LOGS).unwrap();
        conn.execute(
            "INSERT INTO logs (ts, ts_nanos, level, target, thread_id, process_uuid) \
             VALUES (1000, 0, 'INFO', 'codex_core', 'thread-1', 'pid:100:x')",
            [],
        )
        .unwrap();
        std::mem::forget(conn); // leave -wal/-shm behind for this process's lifetime

        let start_time = MockStartTime(HashMap::from([(100, 500)]));
        assert!(is_thread_alive(&home, "thread-1", &start_time));
    }
}
