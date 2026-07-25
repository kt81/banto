//! Session cache mirror and FTS5 full-text search.

use std::collections::HashSet;
use std::path::PathBuf;

use rusqlite::params;

use banto_core::model::{SessionId, SessionMeta};

use super::{Store, StoreError, system_time_to_unix_ms, unix_ms_to_system_time};

impl Store {
    /// Mirrors provider discovery into the cache: upserts every given session
    /// and deletes cached rows (and their FTS entries) whose id is no longer
    /// present. Pins, groups, and panes are intentionally left untouched so
    /// user data survives a source that is temporarily unavailable.
    ///
    /// Paths are stored lossily (see the module docs): non-UTF-8 components
    /// become U+FFFD replacement characters.
    pub fn sync_sessions(&mut self, sessions: &[SessionMeta]) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;

        let keep: HashSet<&str> = sessions.iter().map(|s| s.id.0.as_str()).collect();
        let stale: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM sessions")?;
            let ids = stmt.query_map([], |row| row.get::<_, String>(0))?;
            ids.filter_map(Result::ok)
                .filter(|id| !keep.contains(id.as_str()))
                .collect()
        };
        for id in &stale {
            tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
            tx.execute("DELETE FROM sessions_fts WHERE id = ?1", [id])?;
        }

        for s in sessions {
            let cwd = s.cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
            let source_path = s.source_path.to_string_lossy().into_owned();
            let size = i64::try_from(s.size).unwrap_or(i64::MAX);
            tx.execute(
                "INSERT INTO sessions (id, provider, title, cwd, source_path, mtime_ms, size, is_agent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                     provider    = excluded.provider,
                     title       = excluded.title,
                     cwd         = excluded.cwd,
                     source_path = excluded.source_path,
                     mtime_ms    = excluded.mtime_ms,
                     size        = excluded.size,
                     is_agent    = excluded.is_agent",
                params![
                    s.id.0,
                    s.provider,
                    s.title,
                    cwd,
                    source_path,
                    system_time_to_unix_ms(s.mtime),
                    size,
                    s.is_agent,
                ],
            )?;
            // FTS5 is kept in sync manually: replace this session's entry.
            tx.execute("DELETE FROM sessions_fts WHERE id = ?1", [&s.id.0])?;
            tx.execute(
                "INSERT INTO sessions_fts (title, cwd, id) VALUES (?1, ?2, ?3)",
                params![s.title, cwd, s.id.0],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Returns all cached sessions, most recently modified first.
    pub fn list_sessions(&self) -> Result<Vec<SessionMeta>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, provider, title, cwd, source_path, mtime_ms, size, is_agent
             FROM sessions ORDER BY mtime_ms DESC, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SessionMeta {
                id: SessionId(row.get(0)?),
                provider: row.get(1)?,
                title: row.get(2)?,
                cwd: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
                source_path: PathBuf::from(row.get::<_, String>(4)?),
                mtime: unix_ms_to_system_time(row.get(5)?),
                size: row.get::<_, i64>(6)?.max(0) as u64,
                is_agent: row.get(7)?,
                // Not persisted in the cache; the provider is the source.
                preview: None,
                continuation_of_uuid: None,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Full-text search over title and cwd, best match first.
    ///
    /// User input is sanitized (see [`build_match_expr`]) so raw punctuation
    /// can never reach the FTS5 query parser; a query with nothing searchable
    /// in it returns an empty result instead of erroring.
    pub fn fts_search(&self, query: &str) -> Result<Vec<SessionId>, StoreError> {
        let Some(expr) = build_match_expr(query) else {
            return Ok(Vec::new());
        };
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM sessions_fts WHERE sessions_fts MATCH ?1 ORDER BY rank")?;
        let rows = stmt.query_map([&expr], |row| row.get::<_, String>(0).map(SessionId))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

/// Builds a safe FTS5 MATCH expression from raw user input.
///
/// Each whitespace-separated token is wrapped in double quotes (embedded
/// quotes doubled, so FTS5 operators like `*`, `-`, `(`, `OR` are treated
/// literally) and suffixed with `*` for prefix matching; tokens are joined
/// with implicit AND. Tokens without any alphanumeric character are dropped —
/// quoting them would produce an empty phrase, and the default unicode61
/// tokenizer never indexes pure punctuation anyway. Returns `None` when
/// nothing searchable remains.
fn build_match_expr(query: &str) -> Option<String> {
    let phrases: Vec<String> = query
        .split_whitespace()
        .filter(|token| token.chars().any(char::is_alphanumeric))
        .map(|token| format!("\"{}\"*", token.replace('"', "\"\"")))
        .collect();
    if phrases.is_empty() {
        None
    } else {
        Some(phrases.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{agent_meta, meta};
    use super::*;

    fn ids(found: &[SessionId]) -> Vec<&str> {
        found.iter().map(|id| id.0.as_str()).collect()
    }

    #[test]
    fn sync_roundtrip_preserves_fields() {
        let mut store = Store::open_in_memory().unwrap();
        let original = meta(
            "s1",
            Some("fix flaky test"),
            Some("C:/Users/kt81/Projects/banto"),
        );
        store
            .sync_sessions(std::slice::from_ref(&original))
            .unwrap();

        let listed = store.list_sessions().unwrap();
        assert_eq!(listed.len(), 1);
        let got = &listed[0];
        assert_eq!(got.id, original.id);
        assert_eq!(got.provider, original.provider);
        assert_eq!(got.title, original.title);
        assert_eq!(got.cwd, original.cwd);
        assert_eq!(got.source_path, original.source_path);
        assert_eq!(got.mtime, original.mtime);
        assert_eq!(got.size, original.size);
        assert_eq!(got.is_agent, original.is_agent);
    }

    #[test]
    fn sync_roundtrip_preserves_is_agent_flag() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .sync_sessions(&[
                meta("interactive", Some("normal session"), None),
                agent_meta("agent", Some("spawned session"), None),
            ])
            .unwrap();

        let listed = store.list_sessions().unwrap();
        let is_agent = |id: &str| listed.iter().find(|s| s.id.0 == id).unwrap().is_agent;
        assert!(!is_agent("interactive"));
        assert!(is_agent("agent"));
    }

    #[test]
    fn sync_upsert_updates_is_agent_flag() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .sync_sessions(&[meta("s1", Some("title"), None)])
            .unwrap();
        store
            .sync_sessions(&[agent_meta("s1", Some("title"), None)])
            .unwrap();

        let listed = store.list_sessions().unwrap();
        assert!(listed[0].is_agent);
    }

    #[test]
    fn sync_upsert_updates_existing_row() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .sync_sessions(&[meta("s1", Some("old title"), None)])
            .unwrap();
        store
            .sync_sessions(&[meta("s1", Some("new title"), None)])
            .unwrap();

        let listed = store.list_sessions().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title.as_deref(), Some("new title"));
        // FTS entry was replaced, not duplicated.
        assert_eq!(ids(&store.fts_search("new").unwrap()), ["s1"]);
        assert!(store.fts_search("old").unwrap().is_empty());
    }

    #[test]
    fn sync_deletes_stale_rows_and_fts_entries() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .sync_sessions(&[
                meta("keep", Some("alpha"), None),
                meta("stale", Some("bravo"), None),
            ])
            .unwrap();
        store
            .sync_sessions(&[meta("keep", Some("alpha"), None)])
            .unwrap();

        let listed = store.list_sessions().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.0, "keep");
        assert!(store.fts_search("bravo").unwrap().is_empty());
        assert_eq!(ids(&store.fts_search("alpha").unwrap()), ["keep"]);
    }

    #[test]
    fn fts_search_japanese_prefix() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .sync_sessions(&[
                meta("jp", Some("検索モジュールの実装"), None),
                meta("en", Some("session search module"), None),
            ])
            .unwrap();

        assert_eq!(ids(&store.fts_search("検索").unwrap()), ["jp"]);
        assert_eq!(ids(&store.fts_search("sear").unwrap()), ["en"]);
    }

    #[test]
    fn fts_search_matches_cwd() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .sync_sessions(&[meta("s1", None, Some("C:/Users/kt81/Projects/banto"))])
            .unwrap();
        assert_eq!(ids(&store.fts_search("banto").unwrap()), ["s1"]);
    }

    #[test]
    fn fts_search_punctuation_does_not_error() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .sync_sessions(&[meta("s1", Some("quoted \"title\" here"), None)])
            .unwrap();

        // Raw FTS5 operators and unbalanced quotes must not reach the parser.
        for query in [
            "\"", "\"\"", "*", "-", "(", ")", "( - * \"", "foo*", "-foo", "(bar", "a\"b", "NEAR",
            "OR", "AND", "NOT",
        ] {
            let result = store.fts_search(query);
            assert!(result.is_ok(), "query {query:?} errored: {result:?}");
        }
        // Purely non-alphanumeric input yields an empty result.
        assert!(store.fts_search("* - ( \"").unwrap().is_empty());
        // Embedded quotes are escaped, not interpreted.
        assert_eq!(ids(&store.fts_search("\"title\"").unwrap()), ["s1"]);
    }

    #[test]
    fn fts_search_empty_query_returns_empty() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.fts_search("").unwrap().is_empty());
        assert!(store.fts_search("   ").unwrap().is_empty());
    }

    #[test]
    fn build_match_expr_shapes() {
        assert_eq!(
            build_match_expr("foo bar"),
            Some("\"foo\"* \"bar\"*".to_string())
        );
        assert_eq!(build_match_expr("a\"b"), Some("\"a\"\"b\"*".to_string()));
        assert_eq!(build_match_expr("(*-"), None);
        assert_eq!(build_match_expr(""), None);
    }
}
