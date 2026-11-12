//! Codex CLI session provider.
//!
//! Discovers sessions by `SELECT`ing over [`CodexHome::threads_db_path`]'s
//! `threads` table — one row per session, no per-file parsing. Tolerant by
//! the same contract as [`super::claude_code`]: a row missing what banto
//! needs is skipped, never turned into an error, and a `threads` database
//! that does not exist yet (Codex never run) degrades to an empty result
//! rather than an error, mirroring [`super::claude_code::ClaudeCodeProvider`]
//! degrading a missing `projects/` directory the same way.
//!
//! Reading a database Codex itself may be writing goes through
//! [`crate::sqlite_ro`] — see that module's doc for the stat-then-choose
//! strategy and what it costs.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use rusqlite::types::ValueRef;

use super::{ProviderError, SessionProvider};
use crate::codex_home::CodexHome;
use crate::sqlite_ro::open_read_only;
#[cfg(test)]
use crate::sqlite_ro::wal_sidecar_path;
use banto_core::model::{AgentKind, SessionId, SessionMeta};

/// Session provider for the Codex CLI.
pub struct CodexProvider {
    codex_home: CodexHome,
}

impl CodexProvider {
    /// Create a provider rooted at `codex_home`.
    pub fn new(codex_home: CodexHome) -> Self {
        Self { codex_home }
    }
}

/// Whether a `threads.source` value marks the thread as a sub-agent's — Codex's
/// own automatic-review child of a session, not something a person converses
/// with. Measured (Codex 0.147 and 0.166-0.167.1): an ordinary session's
/// `source` is plain text (`cli`; `codex exec` records `exec`), while a
/// sub-agent's is a JSON object in the same TEXT column
/// (`{"subagent":{"other":"guardian"}}`). `thread_source` is the wrong key — it
/// is newer, nullable, and its value for the same thread already changed between
/// those versions (`subagent` on 0.147, `guardian_review` later) — and so is the
/// title, which is only text a product chose.
///
/// The rule points one way on purpose: only a value positively recognised as a
/// sub-agent's hides a row. Plain text of any value (one banto has never seen
/// included), an empty value, a JSON object *without* a `subagent` key, text that
/// does not parse, and a value that is not text at all all stay listed — hiding a
/// session the operator really started costs far more than showing a sub-agent
/// thread that a future Codex shapes differently.
///
/// Takes the cell as a [`ValueRef`], decided inside the row mapper: reading it
/// into a `String` (or an owned `Value`, which converts text through UTF-8) fails
/// on a TEXT cell holding invalid UTF-8, and a failed read drops the whole row
/// before this could answer "not a sub-agent". Invalid bytes simply do not parse.
fn is_subagent_source(source: ValueRef<'_>) -> bool {
    let ValueRef::Text(bytes) = source else {
        return false;
    };
    matches!(
        serde_json::from_slice::<serde_json::Value>(bytes),
        Ok(serde_json::Value::Object(map)) if map.contains_key("subagent")
    )
}

/// A TEXT column read leniently: invalid UTF-8 degrades to replacement
/// characters instead of failing the read, which would drop a whole row for a
/// column the row can do without. Only for the descriptive columns (`title`,
/// `cwd`, `first_user_message`); `id` and `rollout_path` stay strict — a lossy id
/// or path would name a different thing, not a damaged label.
fn lossy_text(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    Ok(match row.get_ref(index)? {
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    })
}

/// The `source` select item for a `threads` table: the column when this Codex
/// version's schema has it, else `NULL` (every row then reads as not a
/// sub-agent's, i.e. everything is listed, as before the column was read). The
/// provider names its other columns outright and fails without them; this one is
/// the filter's alone, so its absence must not take discovery down.
fn source_column(conn: &Connection) -> &'static str {
    let has_source = conn
        .query_row(
            "SELECT 1 FROM pragma_table_info('threads') WHERE name = 'source'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if has_source { "source" } else { "NULL" }
}

impl SessionProvider for CodexProvider {
    fn name(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn discover(&self) -> Result<Vec<SessionMeta>, ProviderError> {
        let db_path = self.codex_home.threads_db_path();
        if !db_path.exists() {
            return Ok(Vec::new());
        }
        let conn = open_read_only(&db_path)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT id, title, cwd, rollout_path, first_user_message, updated_at_ms, archived, {} \
             FROM threads",
            source_column(&conn)
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                lossy_text(row, 1)?,
                lossy_text(row, 2)?,
                row.get::<_, Option<String>>(3)?,
                lossy_text(row, 4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<bool>>(6)?,
                // Decided here, from whatever the cell holds: a typed read would
                // fail on a non-text or non-UTF-8 value and the row would be
                // skipped below — hiding a session for a reason that has nothing
                // to do with sub-agents.
                is_subagent_source(row.get_ref(7)?),
            ))
        })?;

        let sessions = rows
            // A row whose column types don't match the query (corrupt or
            // unexpectedly-shaped) is skipped, not fatal to the whole scan.
            .flatten()
            .filter_map(
                |(
                    id,
                    title,
                    cwd,
                    rollout_path,
                    first_user_message,
                    updated_at_ms,
                    archived,
                    is_subagent,
                )| {
                    if is_subagent {
                        return None;
                    }
                    // `id` and `rollout_path` are what banto cannot function
                    // without; everything else degrades gracefully instead of
                    // dropping the row (see session_meta_from_row for size).
                    // `archived` unset (a row from before `codex archive`
                    // existed) reads as not archived, not "unknown" — the
                    // same "no signal means visible" default `is_agent` uses.
                    Some(session_meta_from_row(
                        id?,
                        title,
                        cwd,
                        rollout_path?,
                        first_user_message,
                        updated_at_ms.unwrap_or_default(),
                        archived.unwrap_or(false),
                    ))
                },
            )
            .collect();
        Ok(sessions)
    }

    fn find_new_sessions(&self, cwd: &Path, since: SystemTime) -> Vec<SessionId> {
        let db_path = self.codex_home.threads_db_path();
        if !db_path.exists() {
            return Vec::new();
        }
        let Ok(conn) = open_read_only(&db_path) else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT id, cwd, updated_at_ms, {} FROM threads WHERE updated_at_ms >= ?1",
            source_column(&conn)
        )) else {
            return Vec::new();
        };
        let since_ms = system_time_to_unix_ms(since);
        let Ok(rows) = stmt.query_map([since_ms], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                lossy_text(row, 1)?,
                row.get::<_, i64>(2)?,
                is_subagent_source(row.get_ref(3)?),
            ))
        }) else {
            return Vec::new();
        };

        let mut matches: Vec<(i64, SessionId)> = rows
            .flatten()
            // A fresh Worker is found by "the oldest thread in its cwd since it
            // launched", and Codex starts a sub-agent thread of its own beside
            // every launch or resume — in the same cwd, at the same moment. Left
            // in, one could be taken for the Worker's session.
            .filter(|(_, _, _, is_subagent)| !is_subagent)
            .filter_map(|(id, raw_cwd, updated_at_ms, _)| {
                let id = id?;
                let matched = raw_cwd
                    .as_deref()
                    .map(normalize_windows_extended_prefix)
                    .is_some_and(|p| p == cwd);
                matched.then_some((updated_at_ms, SessionId(id)))
            })
            .collect();
        matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.0.cmp(&b.1.0)));
        matches.into_iter().map(|(_, id)| id).collect()
    }
}

/// Build a [`SessionMeta`] from one `threads` row's already-extracted
/// columns.
fn session_meta_from_row(
    id: String,
    title: Option<String>,
    cwd: Option<String>,
    rollout_path: String,
    first_user_message: Option<String>,
    updated_at_ms: i64,
    archived: bool,
) -> SessionMeta {
    let rollout_path = PathBuf::from(rollout_path);
    // The one place this isn't just a SELECT: a missing rollout file (the
    // session was pruned, or the path is stale) must not drop the row —
    // only its size, which degrades to 0 rather than aborting the mapping.
    let size = fs::metadata(&rollout_path).map(|m| m.len()).unwrap_or(0);
    SessionMeta {
        id: SessionId(id),
        agent: AgentKind::Codex,
        title: title.filter(|t| !t.is_empty()),
        cwd: cwd.as_deref().map(normalize_windows_extended_prefix),
        source_path: rollout_path,
        mtime: unix_ms_to_system_time(updated_at_ms),
        size,
        // No equivalent signal exists yet — see SessionMeta::is_agent's doc
        // for why false, not a guess, is the correct value here.
        is_agent: false,
        preview: first_user_message.filter(|p| !p.is_empty()),
        // Codex has no auto-compaction-continuation concept banto tracks —
        // not "unknown", genuinely nothing to substitute.
        continuation_of_uuid: None,
        source_archived: archived,
    }
}

/// Strip the Windows extended-length path prefix (`\\?\`) `threads.cwd`
/// sometimes carries. A rollout file's own recorded cwd does not carry it,
/// so left unstripped this would silently fail to compare equal to a cwd
/// from anywhere else in banto. Normalized once, here, at the point a raw
/// column value becomes a path banto holds — not inside a narrower helper
/// only one caller would reach — so every comparison anyone writes later
/// sees the same value.
fn normalize_windows_extended_prefix(raw: &str) -> PathBuf {
    PathBuf::from(raw.strip_prefix(r"\\?\").unwrap_or(raw))
}

fn unix_ms_to_system_time(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}

fn system_time_to_unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(err) => {
            let before = err.duration();
            i64::try_from(before.as_millis())
                .map(|ms| ms.saturating_neg())
                .unwrap_or(i64::MIN)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A representative `threads` schema: the six columns this provider
    /// reads, plus a few more no real Codex database would be missing, to
    /// prove discovery selects columns by name rather than relying on
    /// `SELECT *`/column order. Never real session data — a hand-authored
    /// synthetic shape only.
    const CREATE_THREADS: &str = "\
        CREATE TABLE threads (\
            id TEXT PRIMARY KEY, \
            title TEXT, \
            cwd TEXT, \
            rollout_path TEXT, \
            first_user_message TEXT, \
            updated_at_ms INTEGER, \
            created_at_ms INTEGER, \
            model TEXT, \
            archived INTEGER DEFAULT 0\
        )";

    /// The same shape with the `source` column newer Codex versions record
    /// (nullable here though Codex's is NOT NULL, so the lenient reading of a
    /// NULL is testable too).
    const CREATE_THREADS_WITH_SOURCE: &str = "\
        CREATE TABLE threads (\
            id TEXT PRIMARY KEY, \
            title TEXT, \
            cwd TEXT, \
            rollout_path TEXT, \
            first_user_message TEXT, \
            updated_at_ms INTEGER, \
            created_at_ms INTEGER, \
            model TEXT, \
            archived INTEGER DEFAULT 0, \
            source TEXT\
        )";

    fn codex_home(dir: &TempDir) -> CodexHome {
        CodexHome::new(dir.path().to_path_buf())
    }

    fn provider(dir: &TempDir) -> CodexProvider {
        CodexProvider::new(codex_home(dir))
    }

    /// One synthetic `threads` row — bundled into a struct rather than
    /// [`write_thread`] taking each column as its own argument, since most
    /// tests only care about one or two fields and leave the rest at their
    /// default.
    struct ThreadRow<'a> {
        id: &'a str,
        title: Option<&'a str>,
        cwd: Option<&'a str>,
        rollout_path: &'a Path,
        first_user_message: Option<&'a str>,
        updated_at_ms: i64,
        archived: bool,
    }

    impl<'a> ThreadRow<'a> {
        fn new(id: &'a str, rollout_path: &'a Path) -> Self {
            Self {
                id,
                title: None,
                cwd: None,
                rollout_path,
                first_user_message: None,
                updated_at_ms: 0,
                archived: false,
            }
        }
    }

    /// Opens a fresh WAL-mode `threads` database at `home`'s db path and
    /// inserts `row`, leaving the connection open (so `-wal`/`-shm` stay
    /// present) unless `close` is true.
    fn write_thread(home: &CodexHome, row: ThreadRow, close: bool) {
        fs::create_dir_all(home.root()).unwrap();
        let conn = Connection::open(home.threads_db_path()).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(CREATE_THREADS).unwrap();
        conn.execute(
            "INSERT INTO threads (id, title, cwd, rollout_path, first_user_message, updated_at_ms, archived) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                row.id,
                row.title,
                row.cwd,
                row.rollout_path.to_string_lossy(),
                row.first_user_message,
                row.updated_at_ms,
                row.archived
            ],
        )
        .unwrap();
        if close {
            drop(conn);
        } else {
            std::mem::forget(conn);
        }
    }

    #[test]
    fn missing_threads_db_yields_empty() {
        let dir = TempDir::new().unwrap();
        assert!(provider(&dir).discover().unwrap().is_empty());
    }

    #[test]
    fn missing_threads_db_yields_empty_find_new_sessions_too() {
        let dir = TempDir::new().unwrap();
        assert!(
            provider(&dir)
                .find_new_sessions(Path::new("/work/proj"), SystemTime::UNIX_EPOCH)
                .is_empty()
        );
    }

    #[test]
    fn discover_maps_a_cold_cleanly_closed_database() {
        // No live writer left behind: -wal is gone after a clean close, so
        // this exercises the immutable=1 open path.
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "hello").unwrap();
        write_thread(
            &home,
            ThreadRow {
                title: Some("A title"),
                cwd: Some("/work/proj"),
                first_user_message: Some("first message"),
                updated_at_ms: 1_700_000_000_000,
                ..ThreadRow::new("thread-1", &rollout)
            },
            true,
        );

        let sessions = provider(&dir).discover().unwrap();
        assert_eq!(sessions.len(), 1);
        let meta = &sessions[0];
        assert_eq!(meta.id, SessionId("thread-1".to_string()));
        assert_eq!(meta.agent, AgentKind::Codex);
        assert_eq!(meta.title.as_deref(), Some("A title"));
        assert_eq!(meta.cwd.as_deref(), Some(Path::new("/work/proj")));
        assert_eq!(meta.source_path, rollout);
        assert_eq!(meta.preview.as_deref(), Some("first message"));
        assert_eq!(meta.size, 5);
        assert!(!meta.is_agent);
        assert_eq!(meta.continuation_of_uuid, None);
        assert!(!meta.source_archived);
        assert_eq!(
            meta.mtime,
            UNIX_EPOCH + Duration::from_millis(1_700_000_000_000)
        );
    }

    #[test]
    fn discover_treats_a_null_archived_column_as_not_archived() {
        // A row from before `codex archive` existed would have never
        // written this column at all; SQLite reports that as NULL, not the
        // schema's own `DEFAULT 0` (which only applies when an INSERT omits
        // the column, not when it's explicitly NULL) — both must degrade to
        // "not archived", not an error or a dropped row.
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "x").unwrap();
        fs::create_dir_all(home.root()).unwrap();
        let conn = Connection::open(home.threads_db_path()).unwrap();
        conn.execute_batch(CREATE_THREADS).unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, archived) VALUES ('thread-1', ?1, NULL)",
            rusqlite::params![rollout.to_string_lossy()],
        )
        .unwrap();
        drop(conn);

        let sessions = provider(&dir).discover().unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(!sessions[0].source_archived);
    }

    #[test]
    fn discover_reads_an_archived_thread() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "x").unwrap();
        write_thread(
            &home,
            ThreadRow {
                archived: true,
                ..ThreadRow::new("thread-1", &rollout)
            },
            true,
        );

        let sessions = provider(&dir).discover().unwrap();
        assert!(sessions[0].source_archived);
    }

    #[test]
    fn discover_reads_a_warm_database_with_an_active_writer() {
        // The connection is deliberately kept open (not dropped): -wal and
        // -shm stay present, so this exercises the mode=ro open path
        // against a database an active writer still holds.
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "hi").unwrap();
        write_thread(
            &home,
            ThreadRow {
                updated_at_ms: 1_700_000_000_000,
                ..ThreadRow::new("thread-warm", &rollout)
            },
            false,
        );
        assert!(
            wal_sidecar_path(&home.threads_db_path()).exists(),
            "setup error: expected the writer to leave -wal behind"
        );

        let sessions = provider(&dir).discover().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, SessionId("thread-warm".to_string()));
    }

    #[test]
    fn cwd_strips_the_windows_extended_length_prefix() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "x").unwrap();
        write_thread(
            &home,
            ThreadRow {
                cwd: Some(r"\\?\C:\work\proj"),
                ..ThreadRow::new("thread-1", &rollout)
            },
            true,
        );

        let sessions = provider(&dir).discover().unwrap();
        assert_eq!(sessions[0].cwd.as_deref(), Some(Path::new(r"C:\work\proj")));
    }

    #[test]
    fn a_missing_rollout_file_lists_the_row_with_zero_size_instead_of_dropping_it() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let missing = dir.path().join("gone.jsonl");
        write_thread(&home, ThreadRow::new("thread-1", &missing), true);

        let sessions = provider(&dir).discover().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].size, 0);
        assert_eq!(sessions[0].source_path, missing);
    }

    #[test]
    fn empty_title_and_preview_normalize_to_none() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "x").unwrap();
        write_thread(
            &home,
            ThreadRow {
                title: Some(""),
                first_user_message: Some(""),
                ..ThreadRow::new("thread-1", &rollout)
            },
            true,
        );

        let sessions = provider(&dir).discover().unwrap();
        assert_eq!(sessions[0].title, None);
        assert_eq!(sessions[0].preview, None);
    }

    #[test]
    fn find_new_sessions_matches_cwd_and_since_oldest_first() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "x").unwrap();
        write_thread(
            &home,
            ThreadRow {
                cwd: Some("/work/proj"),
                updated_at_ms: 1_000,
                ..ThreadRow::new("older", &rollout)
            },
            true,
        );
        {
            let conn = Connection::open(home.threads_db_path()).unwrap();
            conn.execute(
                "INSERT INTO threads (id, cwd, rollout_path, updated_at_ms) \
                 VALUES ('newer', '/work/proj', ?1, 2000)",
                rusqlite::params![rollout.to_string_lossy()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO threads (id, cwd, rollout_path, updated_at_ms) \
                 VALUES ('other-cwd', '/somewhere/else', ?1, 3000)",
                rusqlite::params![rollout.to_string_lossy()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO threads (id, cwd, rollout_path, updated_at_ms) \
                 VALUES ('too-old', '/work/proj', ?1, 500)",
                rusqlite::params![rollout.to_string_lossy()],
            )
            .unwrap();
        }

        let found = provider(&dir).find_new_sessions(
            Path::new("/work/proj"),
            UNIX_EPOCH + Duration::from_millis(1_000),
        );
        assert_eq!(
            found,
            vec![
                SessionId("older".to_string()),
                SessionId("newer".to_string())
            ]
        );
    }

    // --- sub-agent threads are not sessions -------------------------------------

    /// A database whose rows are `(id, source)`, all in `/work/proj`, with a
    /// distinct `updated_at_ms` each (their order is the list's order).
    fn write_sourced(home: &CodexHome, rollout: &Path, rows: &[(&str, rusqlite::types::Value)]) {
        fs::create_dir_all(home.root()).unwrap();
        let conn = Connection::open(home.threads_db_path()).unwrap();
        conn.execute_batch(CREATE_THREADS_WITH_SOURCE).unwrap();
        for (i, (id, source)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO threads (id, cwd, rollout_path, updated_at_ms, source) \
                 VALUES (?1, '/work/proj', ?2, ?3, ?4)",
                rusqlite::params![id, rollout.to_string_lossy(), 1_000 + i as i64, source],
            )
            .unwrap();
        }
    }

    fn text(s: &str) -> rusqlite::types::Value {
        rusqlite::types::Value::Text(s.to_string())
    }

    /// Every source shape the filter must tell apart, as `(id, value, hidden)`.
    fn source_cases() -> Vec<(&'static str, rusqlite::types::Value, bool)> {
        use rusqlite::types::Value;
        vec![
            ("plain-cli", text("cli"), false),
            ("plain-exec", text("exec"), false),
            ("plain-unseen", text("some-future-source"), false),
            ("empty", text(""), false),
            (
                "object-without-the-key",
                text(r#"{"other":"guardian"}"#),
                false,
            ),
            ("empty-object", text("{}"), false),
            ("garbage", text("{not json"), false),
            ("json-string", text(r#""subagent""#), false),
            ("json-array", text(r#"[{"subagent":1}]"#), false),
            ("not-text-integer", Value::Integer(7), false),
            ("not-text-null", Value::Null, false),
            ("not-text-real", Value::Real(1.5), false),
            (
                "not-text-blob",
                Value::Blob(b"{\"subagent\":1}".to_vec()),
                false,
            ),
            (
                "guardian",
                text(r#"{"subagent":{"other":"guardian"}}"#),
                true,
            ),
            ("subagent-string", text(r#"{"subagent":"review"}"#), true),
            (
                "subagent-other",
                text(r#"{"subagent":{"other":"x"}}"#),
                true,
            ),
        ]
    }

    #[test]
    fn discover_hides_exactly_the_threads_that_are_recognised_sub_agents() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "x").unwrap();
        let cases = source_cases();
        let rows: Vec<_> = cases.iter().map(|(id, v, _)| (*id, v.clone())).collect();
        write_sourced(&home, &rollout, &rows);

        let mut listed: Vec<String> = provider(&dir)
            .discover()
            .unwrap()
            .into_iter()
            .map(|meta| meta.id.0)
            .collect();
        listed.sort();
        let mut expected: Vec<String> = cases
            .iter()
            .filter(|(_, _, hidden)| !hidden)
            .map(|(id, _, _)| id.to_string())
            .collect();
        expected.sort();
        assert_eq!(listed, expected);
    }

    #[test]
    fn find_new_sessions_never_returns_a_sub_agent_thread() {
        // The Worker-discovery path: a guardian thread created beside a launch
        // sorts first (oldest) and must not be taken for the Worker's session.
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "x").unwrap();
        write_sourced(
            &home,
            &rollout,
            &[
                ("guardian", text(r#"{"subagent":{"other":"guardian"}}"#)),
                ("workers-own", text("cli")),
                ("exec-run", text("exec")),
                ("odd", text("{not json")),
            ],
        );
        let found = provider(&dir).find_new_sessions(Path::new("/work/proj"), UNIX_EPOCH);
        assert_eq!(
            found,
            vec![
                SessionId("workers-own".to_string()),
                SessionId("exec-run".to_string()),
                SessionId("odd".to_string())
            ]
        );
    }

    #[test]
    fn a_schema_without_the_source_column_lists_everything_as_before() {
        // The legacy fixture has no `source` column.
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "x").unwrap();
        write_thread(
            &home,
            ThreadRow {
                cwd: Some("/work/proj"),
                ..ThreadRow::new("old-schema", &rollout)
            },
            true,
        );
        assert_eq!(provider(&dir).discover().unwrap().len(), 1);
        assert_eq!(
            provider(&dir).find_new_sessions(Path::new("/work/proj"), UNIX_EPOCH),
            vec![SessionId("old-schema".to_string())]
        );
    }

    #[test]
    fn a_source_that_is_text_but_not_utf8_leaves_the_session_listed() {
        // SQLite stores whatever bytes it is given under TEXT affinity; a typed
        // read of such a cell fails, and a failed read used to drop the row.
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "x").unwrap();
        write_sourced(&home, &rollout, &[("fine", text("cli"))]);
        {
            let conn = Connection::open(home.threads_db_path()).unwrap();
            conn.execute(
                "INSERT INTO threads (id, cwd, rollout_path, updated_at_ms, source) \
                 VALUES ('bad-utf8', '/work/proj', ?1, 2000, CAST(X'FF' AS TEXT))",
                rusqlite::params![rollout.to_string_lossy()],
            )
            .unwrap();
        }
        let mut ids: Vec<String> = provider(&dir)
            .discover()
            .unwrap()
            .into_iter()
            .map(|meta| meta.id.0)
            .collect();
        ids.sort();
        assert_eq!(ids, ["bad-utf8", "fine"]);
        let found = provider(&dir).find_new_sessions(Path::new("/work/proj"), UNIX_EPOCH);
        assert_eq!(
            found,
            vec![
                SessionId("fine".to_string()),
                SessionId("bad-utf8".to_string())
            ]
        );
    }

    #[test]
    fn invalid_utf8_in_a_descriptive_column_degrades_it_and_keeps_the_row() {
        let dir = TempDir::new().unwrap();
        let home = codex_home(&dir);
        let rollout = dir.path().join("rollout.jsonl");
        fs::write(&rollout, "x").unwrap();
        write_sourced(&home, &rollout, &[]);
        {
            let conn = Connection::open(home.threads_db_path()).unwrap();
            conn.execute(
                "INSERT INTO threads (id, title, cwd, first_user_message, rollout_path, \
                 updated_at_ms, source) VALUES ('damaged', CAST(X'41FF' AS TEXT), \
                 CAST(X'2F77FF' AS TEXT), CAST(X'42FF' AS TEXT), ?1, 1, 'cli')",
                rusqlite::params![rollout.to_string_lossy()],
            )
            .unwrap();
        }
        let sessions = provider(&dir).discover().unwrap();
        assert_eq!(sessions.len(), 1, "the row survives");
        assert_eq!(sessions[0].title.as_deref(), Some("A\u{FFFD}"));
        assert_eq!(sessions[0].preview.as_deref(), Some("B\u{FFFD}"));
        assert!(sessions[0].cwd.is_some());
    }
}
