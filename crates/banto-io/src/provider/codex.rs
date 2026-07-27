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

#[cfg(test)]
use rusqlite::Connection;

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
        let mut stmt = conn.prepare(
            "SELECT id, title, cwd, rollout_path, first_user_message, updated_at_ms \
             FROM threads",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })?;

        let sessions = rows
            // A row whose column types don't match the query (corrupt or
            // unexpectedly-shaped) is skipped, not fatal to the whole scan.
            .flatten()
            .filter_map(
                |(id, title, cwd, rollout_path, first_user_message, updated_at_ms)| {
                    // `id` and `rollout_path` are what banto cannot function
                    // without; everything else degrades gracefully instead of
                    // dropping the row (see session_meta_from_row for size).
                    Some(session_meta_from_row(
                        id?,
                        title,
                        cwd,
                        rollout_path?,
                        first_user_message,
                        updated_at_ms.unwrap_or_default(),
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
        let Ok(mut stmt) =
            conn.prepare("SELECT id, cwd, updated_at_ms FROM threads WHERE updated_at_ms >= ?1")
        else {
            return Vec::new();
        };
        let since_ms = system_time_to_unix_ms(since);
        let Ok(rows) = stmt.query_map([since_ms], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        }) else {
            return Vec::new();
        };

        let mut matches: Vec<(i64, SessionId)> = rows
            .flatten()
            .filter_map(|(id, raw_cwd, updated_at_ms)| {
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
            "INSERT INTO threads (id, title, cwd, rollout_path, first_user_message, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                row.id,
                row.title,
                row.cwd,
                row.rollout_path.to_string_lossy(),
                row.first_user_message,
                row.updated_at_ms
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
        assert_eq!(
            meta.mtime,
            UNIX_EPOCH + Duration::from_millis(1_700_000_000_000)
        );
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
}
