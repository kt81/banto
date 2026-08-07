//! Lenient parsing of `<claude_home>/sessions/<pid>.json` live-state files.
//!
//! These files are written by Claude Code and their format is undocumented;
//! we capture only the fields we need and skip anything that does not parse.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Live state of one running Claude Code session, read from
/// `<claude_home>/sessions/<pid>.json`.
///
/// Only the fields banto needs are captured; everything else in the file is
/// ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    /// Process id of the running session. Taken from the JSON `pid` field,
    /// falling back to the file stem (`<pid>.json`) when absent.
    pub pid: u32,
    /// Session UUID (JSON key `sessionId`).
    pub session_id: Option<String>,
    /// Working directory of the session.
    pub cwd: Option<PathBuf>,
    /// Reported status, e.g. `"busy"`.
    pub status: Option<String>,
    /// Session kind: `"interactive"` or `"bg"`.
    pub kind: Option<String>,
    /// Human-readable session name, if any.
    pub name: Option<String>,
    /// The process's own kernel-reported start time, as Claude Code recorded
    /// it (JSON key `procStart`) — on Linux/WSL this is `/proc/<pid>/stat`'s
    /// `starttime` field (clock ticks since boot), confirmed by direct,
    /// on-device comparison; its meaning on other platforms is unverified.
    /// Kept as the raw string (never parsed here — this module's parsing is
    /// lenient by contract, and interpreting the value is
    /// `status::probe::SysinfoProbe::is_alive_matching`'s job), so a missing
    /// or unrecognized value degrades to `None` rather than to a parse
    /// error.
    pub proc_start: Option<String>,
    /// Version of the Claude binary running this session (JSON key
    /// `version`, observed 2026-08-07).  The live-state writer keeps this
    /// session fact stable across status rewrites, so it can differ from an
    /// update now present on disk; it is intentionally not rendered by the
    /// TUI.
    pub version: Option<String>,
}

/// Raw serde target. Unknown fields are ignored (serde default behavior);
/// a captured field with an unexpected type fails deserialization, which
/// makes the whole file get skipped.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLiveSession {
    pid: Option<u32>,
    session_id: Option<String>,
    cwd: Option<PathBuf>,
    status: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    proc_start: Option<String>,
    version: Option<String>,
}

/// Read every `*.json` file in `sessions_dir` as a [`LiveSession`].
///
/// Tolerant by contract: a missing or unreadable directory yields an empty
/// vec; non-JSON files, unreadable files, broken JSON, and files whose pid
/// cannot be determined are silently skipped. This function never errors.
pub fn read_live_sessions(sessions_dir: &Path) -> Vec<LiveSession> {
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_json = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        if !is_json {
            continue;
        }
        if let Some(session) = parse_live_session_file(&path) {
            sessions.push(session);
        }
    }
    sessions
}

/// Parse one live-state file, returning `None` on any problem.
fn parse_live_session_file(path: &Path) -> Option<LiveSession> {
    let text = fs::read_to_string(path).ok()?;
    let raw: RawLiveSession = serde_json::from_str(&text).ok()?;
    let pid = raw.pid.or_else(|| pid_from_stem(path))?;
    Some(LiveSession {
        pid,
        session_id: raw.session_id,
        cwd: raw.cwd,
        status: raw.status,
        kind: raw.kind,
        name: raw.name,
        proc_start: raw.proc_start,
        version: raw.version,
    })
}

/// Parse the file stem (`<pid>.json`) as a pid.
fn pid_from_stem(path: &Path) -> Option<u32> {
    path.file_stem()?.to_str()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// Write a synthetic fixture file into `dir`.
    fn write(dir: &Path, file_name: &str, contents: &str) {
        fs::write(dir.join(file_name), contents).unwrap();
    }

    #[test]
    fn parses_valid_file_with_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "1234.json",
            r#"{
                "pid": 1234,
                "sessionId": "00000000-0000-4000-8000-000000000001",
                "cwd": "/tmp/project",
                "status": "busy",
                "kind": "interactive",
                "name": "fixture session",
                "procStart": "1105463",
                "version": "2.1.224"
            }"#,
        );

        let sessions = read_live_sessions(dir.path());
        assert_eq!(
            sessions,
            vec![LiveSession {
                pid: 1234,
                session_id: Some("00000000-0000-4000-8000-000000000001".into()),
                cwd: Some(PathBuf::from("/tmp/project")),
                status: Some("busy".into()),
                kind: Some("interactive".into()),
                name: Some("fixture session".into()),
                proc_start: Some("1105463".into()),
                version: Some("2.1.224".into()),
            }]
        );
    }

    #[test]
    fn ignores_extra_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "42.json",
            r#"{
                "pid": 42,
                "sessionId": "00000000-0000-4000-8000-000000000002",
                "updatedAt": "2026-07-19T00:00:00Z",
                "someFutureField": {"nested": [1, 2, 3]},
                "anotherOne": null
            }"#,
        );

        let sessions = read_live_sessions(dir.path());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pid, 42);
        assert_eq!(
            sessions[0].session_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000002")
        );
    }

    #[test]
    fn skips_broken_json() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "1.json", "{ this is not json");
        write(dir.path(), "2.json", "");
        write(dir.path(), "3.json", r#"{"pid": 3}"#);

        let sessions = read_live_sessions(dir.path());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pid, 3);
    }

    #[test]
    fn skips_file_with_wrong_typed_captured_field() {
        let dir = tempfile::tempdir().unwrap();
        // `sessionId` is a number instead of a string: captured field with an
        // unexpected type, so the file is skipped (never an error).
        write(dir.path(), "7.json", r#"{"pid": 7, "sessionId": 12345}"#);

        assert!(read_live_sessions(dir.path()).is_empty());
    }

    #[test]
    fn missing_optional_fields_become_none() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "99.json", r#"{"pid": 99}"#);

        let sessions = read_live_sessions(dir.path());
        assert_eq!(
            sessions,
            vec![LiveSession {
                pid: 99,
                session_id: None,
                cwd: None,
                status: None,
                kind: None,
                name: None,
                proc_start: None,
                version: None,
            }]
        );
    }

    #[test]
    fn proc_start_is_none_when_the_field_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "99.json", r#"{"pid": 99}"#);

        let sessions = read_live_sessions(dir.path());
        assert_eq!(sessions[0].proc_start, None);
    }

    #[test]
    fn proc_start_is_captured_as_the_raw_string_unparsed() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "99.json",
            r#"{"pid": 99, "procStart": "1105463"}"#,
        );

        let sessions = read_live_sessions(dir.path());
        assert_eq!(sessions[0].proc_start.as_deref(), Some("1105463"));
    }

    #[test]
    fn skips_file_with_wrong_typed_proc_start() {
        // Same leniency contract as every other captured field (see
        // `skips_file_with_wrong_typed_captured_field`): a `procStart` that
        // isn't a string invalidates the whole record rather than silently
        // becoming `None`.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "99.json",
            r#"{"pid": 99, "procStart": 1105463}"#,
        );

        assert!(read_live_sessions(dir.path()).is_empty());
    }

    #[test]
    fn pid_falls_back_to_file_stem() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "4242.json", r#"{"status": "busy"}"#);

        let sessions = read_live_sessions(dir.path());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pid, 4242);
        assert_eq!(sessions[0].status.as_deref(), Some("busy"));
    }

    #[test]
    fn skips_file_without_any_pid() {
        let dir = tempfile::tempdir().unwrap();
        // No `pid` field and a non-numeric stem: no way to determine the pid.
        write(dir.path(), "not-a-pid.json", r#"{"status": "busy"}"#);

        assert!(read_live_sessions(dir.path()).is_empty());
    }

    #[test]
    fn ignores_non_json_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "123.jsonl", r#"{"pid": 123}"#);
        write(dir.path(), "readme.txt", "not a session");
        write(dir.path(), "456", r#"{"pid": 456}"#);
        write(dir.path(), "789.json", r#"{"pid": 789}"#);

        let sessions = read_live_sessions(dir.path());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pid, 789);
    }

    #[test]
    fn missing_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");

        assert!(read_live_sessions(&missing).is_empty());
    }
}
