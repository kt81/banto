//! Schema migrations, keyed by `PRAGMA user_version`.
//!
//! `MIGRATIONS[i]` upgrades a database at `user_version == i` to `i + 1`.
//! Each script runs inside one transaction together with the version bump, so
//! a crash mid-migration leaves the previous version intact and reopening the
//! same file is idempotent.

use rusqlite::Connection;

use super::StoreError;

const MIGRATIONS: &[&str] = &[
    // v1: initial schema.
    //
    // `sessions` is a cache mirror of provider discovery; `sessions_fts` is a
    // contentless-sync FTS5 index over (title, cwd) kept in sync manually on
    // upsert/delete. `pins`, `groups`/`group_members`, and `panes` are
    // banto-owned state referencing sessions loosely (no foreign keys), so
    // user data survives a source that is temporarily unavailable.
    "CREATE TABLE sessions (
        id          TEXT PRIMARY KEY,
        provider    TEXT NOT NULL,
        title       TEXT,
        cwd         TEXT,
        source_path TEXT NOT NULL,
        mtime_ms    INTEGER NOT NULL,
        size        INTEGER NOT NULL
    );
    CREATE VIRTUAL TABLE sessions_fts USING fts5(title, cwd, id UNINDEXED);
    CREATE TABLE pins (
        session_id   TEXT PRIMARY KEY,
        pinned_at_ms INTEGER NOT NULL
    );
    CREATE TABLE groups (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        name       TEXT NOT NULL UNIQUE,
        sort_order INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE group_members (
        group_id   INTEGER NOT NULL,
        session_id TEXT NOT NULL,
        PRIMARY KEY (group_id, session_id)
    );
    CREATE TABLE panes (
        session_id   TEXT PRIMARY KEY,
        backend      TEXT NOT NULL,
        target       TEXT NOT NULL,
        pid          INTEGER,
        opened_at_ms INTEGER NOT NULL
    );",
    // v2: track whether a session was run by a spawned agent (subagent /
    // Agent-Teams teammate) rather than started interactively.
    "ALTER TABLE sessions ADD COLUMN is_agent INTEGER NOT NULL DEFAULT 0;",
    // v3: archived sessions (soft-hide; the source file under ~/.claude is
    // never touched). Mirrors `pins`: a loose reference (no foreign key), so
    // an archived id survives a source that is temporarily unavailable.
    "CREATE TABLE archived (
        session_id     TEXT PRIMARY KEY,
        archived_at_ms INTEGER NOT NULL
    );",
    // v4: brigades — an internal operational cell of one Director session and
    // one or more Worker sessions, hosted together as tiled panes in the
    // emporium mode. A *separate* concept from groups (which are the user's own
    // project/phase filing): a brigade is a live operational unit. Like
    // groups/pins, a loose reference (no foreign keys) so a brigade survives a
    // source that is temporarily unavailable. A session belongs to at most one
    // brigade; that single-membership invariant (and "exactly one Director per
    // brigade") is layered in code, not a schema constraint.
    "CREATE TABLE brigades (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        name          TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    );
    CREATE TABLE brigade_members (
        brigade_id INTEGER NOT NULL,
        session_id TEXT NOT NULL,
        role       TEXT NOT NULL,
        PRIMARY KEY (brigade_id, session_id)
    );",
    // v5: the brigade message queue — the medium banto mediates Director<->Worker
    // over. Written and read by the `banto _mcp` server(s) that embedded claude
    // sessions connect to (see crate::mcp), sharing this same sqlite file with
    // the TUI process (exactly the cross-process access the busy_timeout was set
    // up for). A message is addressed to a *role* within a brigade (so a Director
    // broadcasts to every Worker); delivery is per-session-pull via a cursor
    // (`brigade_cursors`) rather than a global delivered flag, so each recipient
    // of a broadcast sees it independently.
    "CREATE TABLE brigade_messages (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        brigade_id    INTEGER NOT NULL,
        from_session  TEXT NOT NULL,
        to_role       TEXT NOT NULL,
        body          TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    );
    CREATE TABLE brigade_cursors (
        session_id   TEXT PRIMARY KEY,
        last_seen_id INTEGER NOT NULL
    );",
    // v6: brigade_cursors keyed by (brigade_id, session_id) instead of
    // session_id alone. brigade_messages.id is one global AUTOINCREMENT
    // sequence shared by every brigade, so a session-only cursor carried over
    // unscoped when a session moved to a different brigade, silently skipping
    // that brigade's messages whose id happened to fall at or below the old
    // cursor. Existing rows are rekeyed under each session's *current*
    // brigade (per `brigade_members`); a session with no current membership
    // has nothing left to resume into, so its cursor is dropped.
    "CREATE TABLE brigade_cursors_v6 (
        brigade_id   INTEGER NOT NULL,
        session_id   TEXT NOT NULL,
        last_seen_id INTEGER NOT NULL,
        PRIMARY KEY (brigade_id, session_id)
    );
    INSERT INTO brigade_cursors_v6 (brigade_id, session_id, last_seen_id)
        SELECT bm.brigade_id, bc.session_id, bc.last_seen_id
        FROM brigade_cursors bc
        JOIN brigade_members bm ON bm.session_id = bc.session_id;
    DROP TABLE brigade_cursors;
    ALTER TABLE brigade_cursors_v6 RENAME TO brigade_cursors;",
    // v7: banto-owned member identity. `brigade_members` moves from being
    // keyed by the Claude session id itself to a stable `member_token`
    // ('director', 'worker-1', 'worker-2', ...) with an optional
    // `claude_session_id` — a brigade Worker is now formed by banto
    // *before* Claude assigns it a session id (auto-spawned, see
    // crate::embedded::emporium), so the id has to be a nullable, filled-in
    // -later column rather than the primary identity. `brigade_messages`
    // keeps its shape; `from_session` now holds a member token going
    // forward (existing rows are left as-is — old session-id attribution on
    // already-queued messages is cosmetic history, not a correctness
    // concern). Existing v6 rows are rekeyed: the sole director row becomes
    // token 'director', worker rows become 'worker-1', 'worker-2', ... in
    // their old session-id order; `brigade_cursors` follows the same
    // mapping, dropping any cursor with no surviving membership (the INNER
    // JOIN naturally excludes it).
    "CREATE TABLE brigade_members_v7 (
        brigade_id        INTEGER NOT NULL,
        member_token      TEXT NOT NULL,
        role              TEXT NOT NULL,
        claude_session_id TEXT,
        PRIMARY KEY (brigade_id, member_token)
    );
    INSERT INTO brigade_members_v7 (brigade_id, member_token, role, claude_session_id)
        SELECT brigade_id, 'director', role, session_id
        FROM brigade_members WHERE role = 'director';
    INSERT INTO brigade_members_v7 (brigade_id, member_token, role, claude_session_id)
        SELECT brigade_id,
               'worker-' || ROW_NUMBER() OVER (PARTITION BY brigade_id ORDER BY session_id),
               role,
               session_id
        FROM brigade_members WHERE role != 'director';
    CREATE TABLE brigade_cursors_v7 (
        brigade_id   INTEGER NOT NULL,
        member_token TEXT NOT NULL,
        last_seen_id INTEGER NOT NULL,
        PRIMARY KEY (brigade_id, member_token)
    );
    INSERT INTO brigade_cursors_v7 (brigade_id, member_token, last_seen_id)
        SELECT bc.brigade_id, bm7.member_token, bc.last_seen_id
        FROM brigade_cursors bc
        JOIN brigade_members_v7 bm7
          ON bm7.brigade_id = bc.brigade_id AND bm7.claude_session_id = bc.session_id;
    DROP TABLE brigade_members;
    ALTER TABLE brigade_members_v7 RENAME TO brigade_members;
    DROP TABLE brigade_cursors;
    ALTER TABLE brigade_cursors_v7 RENAME TO brigade_cursors;",
    // v8: addressed sends. `brigade_messages` gains a nullable `to_member`
    // column narrowing a role-broadcast down to one specific member token —
    // so with 2+ Workers, an instruction meant for `worker-1` no longer also
    // nudges and occupies `worker-2`. A plain `ADD COLUMN` needs no table
    // rebuild; every existing row is left NULL, which keeps meaning exactly
    // what it always meant: broadcast to every member of `to_role`.
    "ALTER TABLE brigade_messages ADD COLUMN to_member TEXT;",
    // v9: one Claude session belongs to at most one member, enforced by the
    // schema rather than by convention. The v4 comment above deliberately
    // left single-membership "layered in code"; a real bug then wrote it
    // wrong (two auto-spawned Workers resolved to the same discovered id,
    // because the emporium's shell-side handle map wasn't rekeyed on
    // discovery, so the resolved id never entered its "already taken" set).
    // The damage a duplicate does is quiet and wide: `brigade_of_claude_
    // session` answers with whichever row sorts first, so an `_mcp`
    // connection can speak as the wrong member; the relay emits one
    // observation per member token and so nudges a single pane twice; and
    // staging the brigade opens one pane where two were expected.
    //
    // Existing duplicates are cleared back to NULL ("spawned, awaiting
    // discovery" — the state a Worker respawns fresh from) on every row but
    // the highest-rowid one, which is the most recent assignment and so the
    // one whose pane actually ended up holding the id.
    "UPDATE brigade_members SET claude_session_id = NULL
     WHERE claude_session_id IS NOT NULL
       AND rowid NOT IN (
           SELECT MAX(rowid) FROM brigade_members
           WHERE claude_session_id IS NOT NULL
           GROUP BY claude_session_id
       );
     CREATE UNIQUE INDEX brigade_members_claude_session
         ON brigade_members (claude_session_id)
         WHERE claude_session_id IS NOT NULL;",
    // v10: session lineage — resolved auto-compaction parent links
    // (child_id -> parent_id), populated by a streaming uuid scan run
    // outside this module (see banto_io's lineage resolution step). Only
    // resolved links are ever written; a scan that finds nothing is
    // retried later, never persisted as a failure. Lineage is immutable
    // once discovered, so this table only ever grows. `parent_id` is
    // indexed because the leaf-following read (newest descendant of a
    // given id) walks child->child, i.e. the reverse of how the link was
    // discovered.
    "CREATE TABLE session_lineage (
        child_id  TEXT PRIMARY KEY,
        parent_id TEXT NOT NULL
    );
    CREATE INDEX session_lineage_parent ON session_lineage (parent_id);",
    // v11: the sessions cache mirror and its FTS5 index were a historical
    // relic — `Store::sync_sessions`/`list_sessions`/`fts_search` had zero
    // production callers (every provider `discover()` call site feeds the
    // UI directly; search runs live through nucleo, not FTS5), so the
    // operator asked to remove the dead tables outright rather than carry
    // them forward. v1 above still creates `sessions`/`sessions_fts`, and v2
    // still alters `sessions` to add `is_agent` — DO NOT "clean up" v1/v2 to
    // match this: a database that has never advanced past v1 still needs
    // those CREATE/ALTER steps to run before this migration can drop what
    // they made, and a fresh database always replays every script from v1
    // forward regardless of what the final schema looks like. Dropping the
    // FTS5 virtual table first cleans up its shadow tables before the table
    // it indexes disappears.
    "DROP TABLE sessions_fts;
    DROP TABLE sessions;",
];

/// Applies all pending migrations. A `user_version` outside the known range
/// (negative or newer than this build) is rejected.
pub(super) fn apply(conn: &Connection) -> Result<(), StoreError> {
    loop {
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 0 || version > MIGRATIONS.len() as i64 {
            return Err(StoreError::UnsupportedSchemaVersion(version));
        }
        let idx = version as usize;
        if idx == MIGRATIONS.len() {
            return Ok(());
        }
        let script = format!(
            "BEGIN;\n{}\nPRAGMA user_version = {};\nCOMMIT;",
            MIGRATIONS[idx],
            idx + 1
        );
        conn.execute_batch(&script)?;
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Store, StoreError};
    use super::MIGRATIONS;
    use banto_core::model::SessionId;

    /// Proves the bundled rusqlite build ships the FTS5 module.
    #[test]
    fn fts5_module_available() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(x);")
            .unwrap();
        conn.execute("INSERT INTO t (x) VALUES ('hello world')", [])
            .unwrap();
        let hits: i64 = conn
            .query_row("SELECT count(*) FROM t WHERE t MATCH 'hello'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn migrations_idempotent_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        // Nested path also exercises parent-directory creation in `open`.
        let db = dir.path().join("nested").join("banto.db");

        {
            let store = Store::open(&db).unwrap();
            store.pin(&SessionId("s1".to_string())).unwrap();
        }

        // Reopening must not re-run migrations or lose data.
        let store = Store::open(&db).unwrap();
        assert_eq!(store.pinned_ids().unwrap(), [SessionId("s1".to_string())]);
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn v1_to_v2_upgrade_preserves_existing_rows_with_is_agent_defaulting_to_zero() {
        // v11 later drops `sessions` entirely (see the last entry in
        // MIGRATIONS), so unlike the other version-transition tests in this
        // file, this one applies the v1 and v2 scripts directly against a
        // raw connection instead of going through `Store::open` (which
        // always advances a database all the way to the latest version) —
        // the ALTER TABLE itself still has to work correctly for every real
        // v1 database on its way through to v11, even though nothing can
        // observe `is_agent` through the public `Store` API once that table
        // is gone.
        let conn = rusqlite::Connection::open_in_memory().unwrap();

        // Build a database as v1 code would have left it: only the v1
        // script applied, a row inserted without an `is_agent` column.
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, provider, title, cwd, source_path, mtime_ms, size)
             VALUES ('s1', 'claude-code', 'title', NULL, 'C:/s1.jsonl', 1000, 42)",
            [],
        )
        .unwrap();

        // Apply the v2 script directly: the pre-existing row survives, and
        // the new column defaults to false.
        conn.execute_batch(MIGRATIONS[1]).unwrap();
        let is_agent: bool = conn
            .query_row("SELECT is_agent FROM sessions WHERE id = 's1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!is_agent);
    }

    #[test]
    fn v1_genesis_database_migrates_all_the_way_to_v11_and_drops_the_sessions_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // Build a database exactly as v1 code would have left it: only the
        // v1 script applied, `user_version` left at 1, with a real row in
        // the table v11 is about to drop.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, provider, title, cwd, source_path, mtime_ms, size)
                 VALUES ('s1', 'claude-code', 'title', NULL, 'C:/s1.jsonl', 1000, 42)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let table_exists = |store: &Store, name: &str| -> bool {
            store
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    [name],
                    |row| row.get(0),
                )
                .unwrap()
        };

        // Opening through Store::open must carry a v1-genesis database all
        // the way to the latest version in one pass, dropping `sessions`
        // and `sessions_fts` along the way (v11).
        let store = Store::open(&db).unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        assert!(!table_exists(&store, "sessions"));
        assert!(!table_exists(&store, "sessions_fts"));

        // Reopening an already-v11 database is a no-op: no error, same
        // version, tables still gone.
        drop(store);
        let store = Store::open(&db).unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        assert!(!table_exists(&store, "sessions"));
        assert!(!table_exists(&store, "sessions_fts"));
    }

    #[test]
    fn v2_to_v3_upgrade_adds_archived_table_and_preserves_existing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // Build a database as v2 code would have left it: v1 + v2 scripts
        // applied, a session with an existing (multi-)group membership,
        // `user_version` left at 2. group_members is untouched by v3, so
        // this also proves the upgrade doesn't disturb it.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(MIGRATIONS[1]).unwrap();
            conn.execute(
                "INSERT INTO groups (id, name) VALUES (1, 'work'), (2, 'play')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO group_members (group_id, session_id) VALUES (1, 's1'), (2, 's1')",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }

        // Opening through Store::open must run the v3 migration: the
        // archived table exists and is usable, and the pre-existing
        // (multi-)group membership rows are untouched.
        let store = Store::open(&db).unwrap();
        assert_eq!(
            store.group_members(1).unwrap(),
            [SessionId("s1".to_string())]
        );
        assert_eq!(
            store.group_members(2).unwrap(),
            [SessionId("s1".to_string())]
        );
        assert!(store.archived_ids().unwrap().is_empty());
        let s1 = SessionId("s1".to_string());
        store.archive_session(&s1).unwrap();
        assert_eq!(store.archived_ids().unwrap(), [s1]);
        // `apply` always brings the database to the latest version, so this
        // tracks MIGRATIONS.len() rather than a hardcoded 3.
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn v3_to_v4_upgrade_adds_brigade_tables_and_preserves_existing_rows() {
        use super::super::BrigadeRole;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // Build a database as v3 code would have left it: v1..=v3 scripts
        // applied, a session archived, `user_version` left at 3. The brigade
        // tables do not exist yet.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(MIGRATIONS[1]).unwrap();
            conn.execute_batch(MIGRATIONS[2]).unwrap();
            conn.execute(
                "INSERT INTO archived (session_id, archived_at_ms) VALUES ('s1', 1000)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }

        // Opening through Store::open must run the v4 migration: the brigade
        // tables exist and are usable, and the pre-existing archived row is
        // untouched.
        let mut store = Store::open(&db).unwrap();
        assert_eq!(store.archived_ids().unwrap(), [SessionId("s1".to_string())]);
        assert!(store.list_brigades().unwrap().is_empty());
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(
                br,
                "director",
                BrigadeRole::Director,
                Some(&SessionId("dir".to_string())),
            )
            .unwrap();
        assert_eq!(
            store
                .brigade_of_claude_session(&SessionId("dir".to_string()))
                .unwrap(),
            Some((br, "director".to_string(), BrigadeRole::Director))
        );
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn v4_to_v5_upgrade_adds_the_message_queue_and_preserves_existing_rows() {
        use super::super::BrigadeRole;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // Build a database as v4 code would have left it: v1..=v4 scripts
        // applied, a brigade with a Director, `user_version` left at 4. The
        // message-queue tables do not exist yet.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for script in &MIGRATIONS[0..4] {
                conn.execute_batch(script).unwrap();
            }
            conn.execute(
                "INSERT INTO brigades (id, name, created_at_ms) VALUES (1, 'cell', 1000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO brigade_members (brigade_id, session_id, role)
                 VALUES (1, 'dir', 'director')",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 4).unwrap();
        }

        // Opening through Store::open must run the v5 migration: the queue is
        // usable, and the pre-existing brigade membership is untouched.
        let mut store = Store::open(&db).unwrap();
        assert_eq!(
            store
                .brigade_of_claude_session(&SessionId("dir".to_string()))
                .unwrap(),
            Some((1, "director".to_string(), BrigadeRole::Director))
        );
        store
            .enqueue_brigade_message(1, "director", BrigadeRole::Worker, None, "hi")
            .unwrap();
        assert_eq!(
            store
                .fetch_brigade_messages(1, "worker-1", BrigadeRole::Worker)
                .unwrap()
                .len(),
            1
        );
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn v5_to_v7_upgrade_carries_the_cursor_fix_through_the_token_rekey() {
        use super::super::BrigadeRole;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // Build a database as v5 code would have left it: v1..=v5 scripts
        // applied, a brigade with a Worker member and an old-shape
        // (session_id-only) cursor row, `user_version` left at 5.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for script in &MIGRATIONS[0..5] {
                conn.execute_batch(script).unwrap();
            }
            conn.execute(
                "INSERT INTO brigades (id, name, created_at_ms) VALUES (1, 'cell', 1000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO brigade_members (brigade_id, session_id, role)
                 VALUES (1, 'w1', 'worker')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO brigade_cursors (session_id, last_seen_id) VALUES ('w1', 7)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 5).unwrap();
        }

        // Opening through Store::open must run both the v6 and v7 migrations:
        // the lone worker becomes token "worker-1" with its claude session id
        // preserved, and its cursor carries over under the new composite key
        // (brigade_id, member_token) — still scoped, so the v6 fix survives
        // the v7 rekey.
        let mut store = Store::open(&db).unwrap();
        assert_eq!(
            store
                .brigade_of_claude_session(&SessionId("w1".to_string()))
                .unwrap(),
            Some((1, "worker-1".to_string(), BrigadeRole::Worker))
        );
        // A message at id <= the carried-over cursor (7) must not resurface.
        store
            .enqueue_brigade_message(
                1,
                "director",
                BrigadeRole::Worker,
                None,
                "old, already seen",
            )
            .unwrap();
        for _ in 1..7 {
            store
                .enqueue_brigade_message(1, "director", BrigadeRole::Worker, None, "filler")
                .unwrap();
        }
        assert!(
            store
                .fetch_brigade_messages(1, "worker-1", BrigadeRole::Worker)
                .unwrap()
                .is_empty(),
            "messages at or before the carried-over cursor must stay hidden"
        );
        // A message enqueued past the cursor is delivered normally.
        store
            .enqueue_brigade_message(1, "director", BrigadeRole::Worker, None, "new")
            .unwrap();
        let got = store
            .fetch_brigade_messages(1, "worker-1", BrigadeRole::Worker)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "new");

        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn v6_to_v7_upgrade_assigns_tokens_and_rekeys_membership_and_cursors() {
        use super::super::BrigadeRole;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // Build a database as v6 code would have left it: v1..=v6 scripts
        // applied, a brigade with a Director and two Workers (old shape:
        // keyed by session_id), each with its own (brigade_id, session_id)
        // cursor, `user_version` left at 6.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for script in &MIGRATIONS[0..6] {
                conn.execute_batch(script).unwrap();
            }
            conn.execute(
                "INSERT INTO brigades (id, name, created_at_ms) VALUES (1, 'cell', 1000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO brigade_members (brigade_id, session_id, role) VALUES
                 (1, 'dir', 'director'),
                 (1, 'alice', 'worker'),
                 (1, 'bob',   'worker')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO brigade_cursors (brigade_id, session_id, last_seen_id) VALUES
                 (1, 'dir',   0),
                 (1, 'alice', 3),
                 (1, 'bob',   5)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 6).unwrap();
        }

        // Opening through Store::open must run the v7 migration: the
        // director becomes token "director", the two workers become
        // "worker-1"/"worker-2" in session-id order ("alice" < "bob"), each
        // keeping its claude session id and its own cursor value.
        let store = Store::open(&db).unwrap();
        assert_eq!(
            store
                .brigade_of_claude_session(&SessionId("dir".to_string()))
                .unwrap(),
            Some((1, "director".to_string(), BrigadeRole::Director))
        );
        assert_eq!(
            store
                .brigade_of_claude_session(&SessionId("alice".to_string()))
                .unwrap(),
            Some((1, "worker-1".to_string(), BrigadeRole::Worker))
        );
        assert_eq!(
            store
                .brigade_of_claude_session(&SessionId("bob".to_string()))
                .unwrap(),
            Some((1, "worker-2".to_string(), BrigadeRole::Worker))
        );

        let cursor = |token: &str| -> i64 {
            store
                .conn
                .query_row(
                    "SELECT last_seen_id FROM brigade_cursors WHERE brigade_id = 1 AND member_token = ?1",
                    [token],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(cursor("director"), 0);
        assert_eq!(cursor("worker-1"), 3);
        assert_eq!(cursor("worker-2"), 5);

        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn v7_to_v8_upgrade_adds_to_member_and_old_rows_stay_broadcast() {
        use super::super::BrigadeRole;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // Build a database as v7 code would have left it: v1..=v7 scripts
        // applied, a brigade with two Workers and a message already queued
        // the old way (no `to_member` column exists yet, so it's implicitly
        // a role-broadcast), `user_version` left at 7.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for script in &MIGRATIONS[0..7] {
                conn.execute_batch(script).unwrap();
            }
            conn.execute(
                "INSERT INTO brigades (id, name, created_at_ms) VALUES (1, 'cell', 1000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO brigade_members (brigade_id, member_token, role, claude_session_id) VALUES
                 (1, 'director', 'director', 'dir'),
                 (1, 'worker-1', 'worker',   'w1'),
                 (1, 'worker-2', 'worker',   'w2')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO brigade_cursors (brigade_id, member_token, last_seen_id) VALUES
                 (1, 'director', 0), (1, 'worker-1', 0), (1, 'worker-2', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO brigade_messages (brigade_id, from_session, to_role, body, created_at_ms)
                 VALUES (1, 'director', 'worker', 'old broadcast', 2000)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 7).unwrap();
        }

        // Opening through Store::open must run the v8 migration: the
        // pre-existing message (no `to_member` at insert time, so it lands
        // NULL) stays visible to BOTH workers as a broadcast, and a freshly
        // enqueued member-addressed message is usable immediately.
        let mut store = Store::open(&db).unwrap();
        assert_eq!(
            store
                .fetch_brigade_messages(1, "worker-1", BrigadeRole::Worker)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .fetch_brigade_messages(1, "worker-2", BrigadeRole::Worker)
                .unwrap()
                .len(),
            1
        );
        store
            .enqueue_brigade_message(
                1,
                "director",
                BrigadeRole::Worker,
                Some("worker-1"),
                "addressed",
            )
            .unwrap();
        assert_eq!(
            store
                .fetch_brigade_messages(1, "worker-1", BrigadeRole::Worker)
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .fetch_brigade_messages(1, "worker-2", BrigadeRole::Worker)
                .unwrap()
                .is_empty()
        );

        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn v8_to_v9_upgrade_clears_a_duplicated_claude_session_id_and_locks_it_out() {
        use super::super::BrigadeRole;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // Build a database as v8 code would have left it after the
        // discovery bug: v1..=v8 applied, and two Workers both claiming the
        // one session id discovery double-assigned them.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for script in &MIGRATIONS[0..8] {
                conn.execute_batch(script).unwrap();
            }
            conn.execute(
                "INSERT INTO brigades (id, name, created_at_ms) VALUES (1, 'cell', 1000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO brigade_members (brigade_id, member_token, role, claude_session_id) VALUES
                 (1, 'director', 'director', 'dir'),
                 (1, 'worker-1', 'worker',   'dup'),
                 (1, 'worker-2', 'worker',   'dup')",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 8).unwrap();
        }

        let mut store = Store::open(&db).unwrap();

        // The later row keeps the id; the earlier one goes back to
        // "awaiting discovery" and respawns fresh next time it's staged.
        let id_of = |store: &Store, token: &str| {
            store
                .brigade_member(1, token)
                .unwrap()
                .unwrap()
                .claude_session_id
        };
        assert_eq!(id_of(&store, "worker-1"), None);
        assert_eq!(
            id_of(&store, "worker-2"),
            Some(SessionId("dup".to_string()))
        );
        assert_eq!(
            id_of(&store, "director"),
            Some(SessionId("dir".to_string()))
        );
        // ...so the id now resolves to exactly one member, not whichever
        // row happened to sort first.
        assert_eq!(
            store
                .brigade_of_claude_session(&SessionId("dup".to_string()))
                .unwrap(),
            Some((1, "worker-2".to_string(), BrigadeRole::Worker))
        );

        // And a fresh duplicate can no longer be written at all: the
        // reassignment moves the id, it never lands in two rows.
        store
            .set_member_claude_session(1, "worker-1", &SessionId("dup".to_string()))
            .unwrap();
        assert_eq!(id_of(&store, "worker-2"), None);
        assert_eq!(
            id_of(&store, "worker-1"),
            Some(SessionId("dup".to_string()))
        );

        // A second member inserted directly with a taken id is rejected by
        // the index rather than silently duplicating.
        assert!(
            store
                .add_brigade_member(
                    1,
                    "worker-3",
                    BrigadeRole::Worker,
                    Some(&SessionId("dup".to_string())),
                )
                .is_err()
        );

        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn v9_to_v10_upgrade_adds_session_lineage_table() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // Build a database as v9 code would have left it: v1..=v9 applied,
        // no session_lineage table yet, user_version left at 9.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for script in &MIGRATIONS[0..9] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 9).unwrap();
        }

        // Opening through Store::open must run the v10 migration: the
        // lineage table exists and is usable.
        let store = Store::open(&db).unwrap();
        assert!(store.lineage_parent_ids().unwrap().is_empty());
        let child = SessionId("child".to_string());
        let parent = SessionId("parent".to_string());
        store.record_lineage(&child, &parent).unwrap();
        assert_eq!(store.lineage_leaf(&parent).unwrap(), child);

        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn future_schema_version_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        drop(Store::open(&db).unwrap());

        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);

        let Err(err) = Store::open(&db) else {
            panic!("open should reject a future schema version");
        };
        assert!(matches!(err, StoreError::UnsupportedSchemaVersion(99)));
    }
}
