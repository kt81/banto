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
    // never touched). Loose reference to `sessions`, same reason as v1.
    "CREATE TABLE archived (
        session_id     TEXT PRIMARY KEY,
        archived_at_ms INTEGER NOT NULL
    );",
    // v4: brigades — an internal operational cell of one Director session and
    // one or more Worker sessions, hosted together as tiled panes in the
    // emporium mode. A *separate* concept from groups (which are the user's own
    // project/phase filing): a brigade is a live operational unit. Loose
    // reference to `sessions`, same reason as v1. A session belongs to at
    // most one brigade; that single-membership invariant (and "exactly one
    // Director per brigade") is layered in code, not a schema constraint.
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
    // v11: drops the sessions cache mirror and its FTS5 index — see
    // `banto_io::store`'s module doc for what the store looks like after
    // this. v1 above still
    // creates `sessions`/`sessions_fts`, and v2 still alters `sessions` to
    // add `is_agent` — DO NOT "clean up" v1/v2 to match this: a database
    // that has never advanced past v1 still needs those CREATE/ALTER steps
    // to run before this migration can drop what they made, and a fresh
    // database always replays every script from v1 forward regardless of
    // what the final schema looks like. Dropping the FTS5 virtual table
    // first cleans up its shadow tables before the table it indexes
    // disappears.
    "DROP TABLE sessions_fts;
    DROP TABLE sessions;",
    // v12: `claude_session_id` renamed back to `session_id` — its v4 name,
    // before v7 attached "claude". Codex became a second product this
    // column has to hold (banto now forms brigades around Codex sessions
    // too), so a name naming only one of them was no longer a fact, just a
    // leftover. `RENAME COLUMN` rewrites the index definition below to
    // reference the new column name automatically, but SQLite has no
    // `ALTER INDEX RENAME`, so the index keeps its now-equally-stale old
    // name until dropped and recreated by hand.
    "ALTER TABLE brigade_members RENAME COLUMN claude_session_id TO session_id;
    DROP INDEX brigade_members_claude_session;
    CREATE UNIQUE INDEX brigade_members_session
        ON brigade_members (session_id)
        WHERE session_id IS NOT NULL;",
    // v13: two nullable columns recording whether/when `banto _hook` (the
    // Codex SessionStart hook process, docs/notes/codex-briefing-spike.md)
    // delivered this member its briefing. Deliberately separate from
    // `session_id` above: that column is identity, unique-indexed; these are
    // an observation banto made about a delivery attempt, and can disagree
    // with `session_id` momentarily without meaning anything is wrong (see
    // `BrigadeMember::briefed_session_id`'s doc). Every gate the spike
    // measured that can eat a briefing (deferred tool, unapproved MCP call,
    // untrusted hook) fails silently, so `briefed_at_ms` staying NULL after a
    // session clearly started is the only signal that happened.
    "ALTER TABLE brigade_members ADD COLUMN briefed_at_ms INTEGER;
    ALTER TABLE brigade_members ADD COLUMN briefed_session_id TEXT;",
    // v14: banto chooses a fresh member's product before that product has
    // necessarily written a discoverable session transcript. Keep old rows
    // NULL so they retain the historical App-row fallback.
    "ALTER TABLE brigade_members ADD COLUMN agent TEXT;",
    // v15: the emporium's arrangement preferences survive a restart.
    //
    // Per-member values (stack position, row weight, collapsed) are columns on
    // `brigade_members`, not a side table keyed by token, because a member row
    // is deleted on dismissal and with its brigade on disband: forgetting a
    // dismissed member's arrangement, and a reused token starting fresh, come
    // with the row for nothing. (A Worker's token is not reused within its
    // brigade — v16's counter — but the Goinkyo's is, by every consultation.)
    // Brigade-level values are columns on `brigades`
    // for the same reason. NULL means "no preference, use the default"; the
    // `minimized` flag has no NULL because "not collapsed" is its default.
    //
    // `ui_prefs` is the store's first key/value table, for what belongs to
    // the whole window rather than a brigade (today: `sidebar_width`).
    "ALTER TABLE brigade_members ADD COLUMN stack_rank INTEGER;
    ALTER TABLE brigade_members ADD COLUMN row_weight INTEGER;
    ALTER TABLE brigade_members ADD COLUMN minimized INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE brigades ADD COLUMN master_permille INTEGER;
    ALTER TABLE brigades ADD COLUMN collapsed_stack_width INTEGER;
    ALTER TABLE brigades ADD COLUMN solo_member TEXT;
    CREATE TABLE ui_prefs (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );",
    // v16: a member's display name, given by its Director (`name_member`), and
    // the per-brigade Worker counter.
    //
    // `display_name`: nullable text on the member's own row for the usual
    // reason: a dismissed member's row is deleted, so its name goes with it. The
    // token stays the identifier; nothing is keyed on this.
    //
    // `brigades.worker_seq`: the highest Worker number ever issued in the
    // brigade, so a Worker token is never reused within a brigade (what
    // AUTOINCREMENT does for a rowid): the next Worker is
    // `max(worker_seq, highest current worker-<n>) + 1` (`Store::add_worker`),
    // and dismissing the highest, or every, Worker no longer hands its number
    // back. Backfilled with the highest `worker-<n>` among the brigade's
    // *current* members, the best that can be known: the numbers of Workers
    // dismissed before this migration were never recorded and cannot be
    // recovered, so such a brigade may still hand one of those out once (only
    // numbers above the highest current Worker, never one a live Worker holds).
    // Only tokens that are exactly `worker-` and up to nine digits count; any
    // other token (hand-edited data) is ignored.
    "ALTER TABLE brigade_members ADD COLUMN display_name TEXT;
    ALTER TABLE brigades ADD COLUMN worker_seq INTEGER NOT NULL DEFAULT 0;
    UPDATE brigades SET worker_seq = COALESCE((
        SELECT MAX(CAST(substr(member_token, 8) AS INTEGER))
        FROM brigade_members
        WHERE brigade_members.brigade_id = brigades.id
          AND member_token GLOB 'worker-[0-9]*'
          AND substr(member_token, 8) NOT GLOB '*[^0-9]*'
          AND length(substr(member_token, 8)) <= 9
    ), 0);",
];

/// Applies all pending migrations. A `user_version` outside the known range
/// (negative or newer than this build) is rejected.
///
/// Several processes open the same file (the emporium, each agent's `banto`
/// MCP server, a second banto), so a migration is decided *under the write
/// lock*: `BEGIN IMMEDIATE` first, then `user_version`, then apply or skip,
/// then commit. Choosing the script from a version read before taking the lock
/// would use a stale answer — the process that waited on the lock would run an
/// `ALTER TABLE ... ADD COLUMN` on a database the other one had already
/// migrated, and fail to open.
///
/// But almost every open finds the file already current, and taking the write
/// lock for that would turn each open into a writer: it would queue behind any
/// write in progress and fail with `SQLITE_BUSY` after the busy timeout where
/// a plain read succeeds. So a lock-free read comes first, and the lock is
/// taken only when there is something to do. That read cannot be wrong in the
/// way the stale one above was: `user_version` only ever increases, so seeing
/// the latest version without the lock means it is the latest version, full
/// stop. Any other answer sends the caller into the locked loop, which
/// re-reads under the lock.
pub(super) fn apply(conn: &Connection) -> Result<(), StoreError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == MIGRATIONS.len() as i64 {
        return Ok(());
    }
    if version < 0 || version > MIGRATIONS.len() as i64 {
        return Err(StoreError::UnsupportedSchemaVersion(version));
    }
    loop {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match apply_one(conn) {
            Ok(done) => {
                conn.execute_batch("COMMIT")?;
                if done {
                    return Ok(());
                }
            }
            Err(err) => {
                // The error is the one to report; a failed rollback adds nothing.
                let _ = conn.execute_batch("ROLLBACK");
                return Err(err);
            }
        }
    }
}

/// With the write lock held: bring the database up one version, or report
/// (`true`) that it is already current.
fn apply_one(conn: &Connection) -> Result<bool, StoreError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version < 0 || version > MIGRATIONS.len() as i64 {
        return Err(StoreError::UnsupportedSchemaVersion(version));
    }
    let idx = version as usize;
    if idx == MIGRATIONS.len() {
        return Ok(true);
    }
    conn.execute_batch(&format!(
        "{}\nPRAGMA user_version = {};",
        MIGRATIONS[idx],
        idx + 1
    ))?;
    Ok(false)
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

        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, provider, title, cwd, source_path, mtime_ms, size)
             VALUES ('s1', 'claude-code', 'title', NULL, 'C:/s1.jsonl', 1000, 42)",
            [],
        )
        .unwrap();

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

        let store = Store::open(&db).unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        assert!(!table_exists(&store, "sessions"));
        assert!(!table_exists(&store, "sessions_fts"));

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
                .brigade_of_session(&SessionId("dir".to_string()))
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

        let mut store = Store::open(&db).unwrap();
        assert_eq!(
            store
                .brigade_of_session(&SessionId("dir".to_string()))
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
                .brigade_of_session(&SessionId("w1".to_string()))
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

        let store = Store::open(&db).unwrap();
        assert_eq!(
            store
                .brigade_of_session(&SessionId("dir".to_string()))
                .unwrap(),
            Some((1, "director".to_string(), BrigadeRole::Director))
        );
        assert_eq!(
            store
                .brigade_of_session(&SessionId("alice".to_string()))
                .unwrap(),
            Some((1, "worker-1".to_string(), BrigadeRole::Worker))
        );
        assert_eq!(
            store
                .brigade_of_session(&SessionId("bob".to_string()))
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
    fn v8_to_v9_upgrade_clears_a_duplicated_session_id_and_locks_it_out() {
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
            store.brigade_member(1, token).unwrap().unwrap().session_id
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
                .brigade_of_session(&SessionId("dup".to_string()))
                .unwrap(),
            Some((1, "worker-2".to_string(), BrigadeRole::Worker))
        );

        // And a fresh duplicate can no longer be written at all: the
        // reassignment moves the id, it never lands in two rows.
        store
            .set_member_session(1, "worker-1", &SessionId("dup".to_string()))
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

        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for script in &MIGRATIONS[0..9] {
                conn.execute_batch(script).unwrap();
            }
            conn.pragma_update(None, "user_version", 9).unwrap();
        }

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
    fn v11_to_v12_upgrade_renames_the_session_column_and_its_index() {
        use super::super::BrigadeRole;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // Build a database as v11 code would have left it: a brigade with a
        // Director already assigned an id and a Worker still awaiting one.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for script in &MIGRATIONS[0..11] {
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
                 (1, 'worker-1', 'worker',   NULL)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 11).unwrap();
        }

        let mut store = Store::open(&db).unwrap();

        // The pre-migration row reads back under the new column name.
        assert_eq!(
            store
                .brigade_member(1, "director")
                .unwrap()
                .unwrap()
                .session_id,
            Some(SessionId("dir".to_string()))
        );

        // The partial unique index, recreated under its new name, still lets
        // any number of NULLs coexist...
        store
            .add_brigade_member(1, "worker-2", BrigadeRole::Worker, None)
            .unwrap();
        assert_eq!(
            store
                .brigade_member(1, "worker-1")
                .unwrap()
                .unwrap()
                .session_id,
            None
        );
        assert_eq!(
            store
                .brigade_member(1, "worker-2")
                .unwrap()
                .unwrap()
                .session_id,
            None
        );
        // ...but still rejects a non-NULL duplicate.
        assert!(
            store
                .add_brigade_member(
                    1,
                    "worker-3",
                    BrigadeRole::Worker,
                    Some(&SessionId("dir".to_string())),
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
    fn v12_to_v14_upgrade_keeps_existing_brigade_member_fields_null() {
        use super::super::BrigadeRole;

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // Build a database as v12 code would have left it: a brigade with a
        // Director already assigned a session id, `user_version` left at 12.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for script in &MIGRATIONS[0..12] {
                conn.execute_batch(script).unwrap();
            }
            conn.execute(
                "INSERT INTO brigades (id, name, created_at_ms) VALUES (1, 'cell', 1000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO brigade_members (brigade_id, member_token, role, session_id) VALUES
                 (1, 'director', 'director', 'dir')",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 12).unwrap();
        }

        let store = Store::open(&db).unwrap();

        // The pre-migration row survives, with every later optional column
        // defaulting to NULL (including the v14 agent kind).
        let director = store.brigade_member(1, "director").unwrap().unwrap();
        assert_eq!(director.role, BrigadeRole::Director);
        assert_eq!(director.session_id, Some(SessionId("dir".to_string())));
        assert_eq!(director.agent, None);
        assert_eq!(director.briefed_at, None);
        assert_eq!(director.briefed_session_id, None);

        // A delivery recorded after the upgrade reads back correctly.
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_millis(5000);
        store
            .record_briefing(1, "director", &SessionId("dir".to_string()), at)
            .unwrap();
        let director = store.brigade_member(1, "director").unwrap().unwrap();
        assert_eq!(director.briefed_at, Some(at));
        assert_eq!(
            director.briefed_session_id,
            Some(SessionId("dir".to_string()))
        );

        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn v14_to_v15_upgrade_keeps_the_rows_and_reads_the_new_columns_as_no_preference() {
        use banto_core::model::BrigadeRole;
        assert!(
            MIGRATIONS.len() >= 15,
            "v15 is one of the migrations it runs"
        );

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        // A database as v14 code left it: a brigade with a Director and a
        // Worker, a pin, and no arrangement columns at all.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for script in &MIGRATIONS[0..14] {
                conn.execute_batch(script).unwrap();
            }
            conn.execute(
                "INSERT INTO brigades (id, name, created_at_ms) VALUES (1, 'cell', 1000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO brigade_members (brigade_id, member_token, role, session_id, agent)
                 VALUES (1, 'director', 'director', 'dir', 'claude'),
                        (1, 'worker-1', 'worker', 'w1', 'codex')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO pins (session_id, pinned_at_ms) VALUES ('w1', 5)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 14).unwrap();
        }

        let store = Store::open(&db).unwrap();

        // Everything that was there still is.
        let members = store.brigade_members(1).unwrap();
        assert_eq!(members.len(), 2);
        let worker = store.brigade_member(1, "worker-1").unwrap().unwrap();
        assert_eq!(worker.role, BrigadeRole::Worker);
        assert_eq!(worker.session_id, Some(SessionId("w1".to_string())));
        assert_eq!(store.pinned_ids().unwrap(), [SessionId("w1".to_string())]);
        // The new columns default to "no preference".
        assert_eq!(
            store.brigade_layout(1).unwrap(),
            banto_core::model::StoredLayout::default()
        );
        assert_eq!(store.sidebar_width().unwrap(), None);
        let (rank, weight, minimized): (Option<i64>, Option<i64>, i64) = store
            .conn
            .query_row(
                "SELECT stack_rank, row_weight, minimized FROM brigade_members
                 WHERE member_token = 'worker-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((rank, weight, minimized), (None, None, 0));

        // And the upgraded store takes writes.
        store.set_sidebar_width(44).unwrap();
        assert_eq!(store.sidebar_width().unwrap(), Some(44));
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn v15_to_v16_upgrade_keeps_the_rows_and_reads_no_display_names() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        // A v15 database: v14's rows, then the v15 script.
        v14_database(&db);
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(MIGRATIONS[14]).unwrap();
            conn.pragma_update(None, "user_version", 15).unwrap();
        }

        let mut store = Store::open(&db).unwrap();
        let members = store.brigade_members(1).unwrap();
        let tokens: Vec<&str> = members.iter().map(|m| m.token.as_str()).collect();
        assert_eq!(tokens, ["director", "worker-1"], "the rows survived");
        assert!(members.iter().all(|m| m.display_name.is_none()));
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);

        // And the upgraded store takes a name.
        assert!(matches!(
            name_it(&mut store, 1, "worker-1", "impl"),
            super::super::MemberNaming::Set { .. }
        ));
        assert_eq!(
            store
                .brigade_member(1, "worker-1")
                .unwrap()
                .unwrap()
                .display_name,
            Some("impl".to_string())
        );
    }

    #[test]
    fn v15_to_v16_backfills_each_brigades_worker_counter_from_its_current_members() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for script in &MIGRATIONS[0..15] {
                conn.execute_batch(script).unwrap();
            }
            for (id, name) in [(1, "gaps"), (2, "odd"), (3, "none"), (4, "wide")] {
                conn.execute(
                    "INSERT INTO brigades (id, name, created_at_ms) VALUES (?1, ?2, 1000)",
                    rusqlite::params![id, name],
                )
                .unwrap();
            }
            let members = [
                // Gaps, out of order: the highest current number is 7.
                (1, "director", "director"),
                (1, "worker-3", "worker"),
                (1, "worker-7", "worker"),
                (1, "worker-1", "worker"),
                // Non-numeric and near-miss tokens are ignored; 12 is the highest.
                (2, "worker-x", "worker"),
                (2, "worker-3a", "worker"),
                (2, "scout", "worker"),
                (2, "worker-", "worker"),
                (2, "Worker-99", "worker"),
                (2, "worker-12", "worker"),
                // No Workers at all.
                (3, "director", "director"),
                (3, "goinkyo", "goinkyo"),
                // Too long to be a number banto issued.
                (4, "worker-12345678901", "worker"),
                (4, "worker-2", "worker"),
            ];
            for (brigade, token, role) in members {
                conn.execute(
                    "INSERT INTO brigade_members (brigade_id, member_token, role)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![brigade, token, role],
                )
                .unwrap();
            }
            conn.pragma_update(None, "user_version", 15).unwrap();
        }

        let mut store = Store::open(&db).unwrap();
        let counters: Vec<(i64, i64)> = {
            let mut stmt = store
                .conn
                .prepare("SELECT id, worker_seq FROM brigades ORDER BY id")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(counters, [(1, 7), (2, 12), (3, 0), (4, 2)]);
        // And the numbering goes on from there.
        assert_eq!(store.add_worker(1, None).unwrap(), "worker-8");
        assert_eq!(store.add_worker(2, None).unwrap(), "worker-13");
        assert_eq!(store.add_worker(3, None).unwrap(), "worker-1");
        assert_eq!(store.add_worker(4, None).unwrap(), "worker-3");
    }

    /// A file-backed database exactly as v14 code left it: a brigade with a
    /// Director and a Worker.
    fn v14_database(path: &std::path::Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        for script in &MIGRATIONS[0..14] {
            conn.execute_batch(script).unwrap();
        }
        conn.execute(
            "INSERT INTO brigades (id, name, created_at_ms) VALUES (1, 'cell', 1000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO brigade_members (brigade_id, member_token, role, session_id)
             VALUES (1, 'director', 'director', 'dir'), (1, 'worker-1', 'worker', 'w1')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 14).unwrap();
    }

    fn name_it(
        store: &mut Store,
        br: banto_core::model::BrigadeId,
        token: &str,
        name: &str,
    ) -> super::super::MemberNaming {
        store.name_member(br, token, name).unwrap()
    }

    fn open_with_timeout(path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        conn
    }

    fn assert_current_and_intact(path: &std::path::Path) {
        let store = Store::open(path).unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        assert_eq!(
            store.brigade_members(1).unwrap().len(),
            2,
            "the data is intact"
        );
    }

    #[test]
    fn a_connection_opened_before_another_migrated_the_file_applies_as_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        v14_database(&db);
        // B is opened first, A migrates the whole file, then B applies.
        let b = open_with_timeout(&db);
        let a = open_with_timeout(&db);
        super::apply(&a).unwrap();

        super::apply(&b).expect("the second process finds nothing to do");

        assert_current_and_intact(&db);
    }

    #[test]
    fn a_process_that_waited_on_the_write_lock_does_not_rerun_what_the_holder_applied() {
        // The interleaving that used to fail: B has chosen its script from a
        // version read before A committed, then gets the lock.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        v14_database(&db);
        let a = open_with_timeout(&db);
        // A holds the write lock with v15 applied but not yet committed.
        a.execute_batch("BEGIN IMMEDIATE").unwrap();
        a.execute_batch(&format!("{}\nPRAGMA user_version = 15;", MIGRATIONS[14]))
            .unwrap();

        let path = db.clone();
        let b = std::thread::spawn(move || {
            let conn = open_with_timeout(&path);
            super::apply(&conn)
        });
        // Long enough for B to have started and be waiting on A.
        std::thread::sleep(std::time::Duration::from_millis(400));
        a.execute_batch("COMMIT").unwrap();

        b.join()
            .unwrap()
            .expect("B waited, saw v15, and did nothing");
        assert_current_and_intact(&db);
    }

    #[test]
    fn several_processes_opening_a_previous_version_store_at_once_all_succeed() {
        for round in 0..8 {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("banto.db");
            v14_database(&db);
            let start = std::sync::Arc::new(std::sync::Barrier::new(4));
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    let (path, start) = (db.clone(), start.clone());
                    std::thread::spawn(move || {
                        let conn = open_with_timeout(&path);
                        start.wait();
                        super::apply(&conn)
                    })
                })
                .collect();
            for thread in threads {
                thread
                    .join()
                    .unwrap()
                    .unwrap_or_else(|err| panic!("round {round}: {err}"));
            }
            assert_current_and_intact(&db);
        }
    }

    #[test]
    fn opening_an_already_current_store_does_not_wait_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        drop(Store::open(&db).unwrap());
        // Another process is mid-write: it holds the write lock, uncommitted.
        let writer = open_with_timeout(&db);
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        writer
            .execute("INSERT INTO ui_prefs (key, value) VALUES ('x', '1')", [])
            .unwrap();

        // The store's busy timeout is 5 s; a plain read must not use any of it.
        let started = std::time::Instant::now();
        let store = Store::open(&db).expect("a current store opens beside a writer");
        let waited = started.elapsed();
        assert!(
            waited < std::time::Duration::from_millis(1500),
            "opened after {waited:?}: it queued behind the writer"
        );
        assert_eq!(store.brigade_members(1).unwrap().len(), 0);
        writer.execute_batch("COMMIT").unwrap();
    }

    #[test]
    fn a_future_version_is_refused_on_the_fast_path_without_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        drop(Store::open(&db).unwrap());
        let conn = open_with_timeout(&db);
        conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 3)
            .unwrap();
        // A writer holds the lock throughout, as in the previous test.
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let Err(err) = Store::open(&db) else {
            panic!("a newer schema must be refused");
        };
        assert!(
            matches!(err, StoreError::UnsupportedSchemaVersion(v) if v == MIGRATIONS.len() as i64 + 3)
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(1500));
        conn.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn the_store_runs_in_the_default_rollback_journal_mode() {
        // Nothing sets a journal mode, so a file-backed store is in DELETE
        // mode: a reader is let in beside a writer that holds only the
        // reserved lock (what `BEGIN IMMEDIATE` takes), and waits only for the
        // brief exclusive moment of a commit.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("banto.db")).unwrap();
        let mode: String = store
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
    }

    #[test]
    fn a_failing_migration_step_is_rolled_back_and_the_lock_released() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        v14_database(&db);
        // Make v15's first ALTER fail: the column already exists.
        let conn = open_with_timeout(&db);
        conn.execute_batch("ALTER TABLE brigade_members ADD COLUMN stack_rank INTEGER")
            .unwrap();
        assert!(super::apply(&conn).is_err());
        // Nothing was half-applied and the connection is not stuck in a transaction.
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 14);
        assert!(conn.is_autocommit());
        let other = open_with_timeout(&db);
        other
            .execute_batch("BEGIN IMMEDIATE; COMMIT;")
            .expect("the write lock is free");
    }

    #[test]
    fn a_database_one_version_ahead_of_the_arrangement_migration_is_still_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        drop(Store::open(&db).unwrap());
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 1)
            .unwrap();
        drop(conn);

        let Err(err) = Store::open(&db) else {
            panic!("a newer schema must be refused");
        };
        assert!(
            matches!(err, StoreError::UnsupportedSchemaVersion(v) if v == MIGRATIONS.len() as i64 + 1)
        );
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
