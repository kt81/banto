//! Read-only access to a Codex sqlite database Codex itself may be actively
//! writing — shared by [`crate::provider::codex`] (`state_5.sqlite`) and
//! [`crate::codex_liveness`] (`logs_2.sqlite`), which independently
//! measured the same answer for their own database before sharing this
//! implementation (P2c re-ran every one of P2a's checks against
//! `logs_2.sqlite` specifically — this session's own scratchpad, plus a
//! read against a real, live `~/.codex/logs_2.sqlite` — rather than assume
//! `state_5.sqlite`'s answer transferred).
//!
//! Correct in every case, and zero-write in every case but one. Stat for
//! the `-wal` sidecar first, then choose: present -> open `mode=ro`;
//! absent -> open `file:...?immutable=1`. Both forms were measured directly
//! (rusqlite 0.40.1 bundled — same version this crate pins): `immutable=1`
//! against a cleanly-checkpointed cold database reads every row with zero
//! filesystem writes; `mode=ro` against a warm database with an active
//! writer sees every commit and, on its own, writes nothing either (the
//! writer's own WAL growth is a separate, expected effect, not this
//! connection's). `immutable=1` alone is not always safe: against an
//! actively-written database it can silently return a stale or incomplete
//! snapshot — exactly the case `-wal`'s presence rules out, which is why
//! the stat comes first.
//!
//! One exception, named rather than reasoned away: opening `mode=ro`
//! against crash residue (`-wal` present, its `-shm` sidecar absent —
//! e.g. Codex was killed mid-write) recreates a fresh `-shm` on that first
//! read, a single ~32KB write into a directory banto otherwise never writes
//! to. It is SQLite's own coordination index, not banto's data, and Codex's
//! own next run would create it regardless — but it is a write, so it is
//! named here and in the README's read-only section rather than smoothed
//! over.
//!
//! This also means one poll's worth of staleness on the TOCTOU path: if a
//! writer starts and commits in the gap between this stat and this open, a
//! database that was cold a moment ago opens `immutable=1` and returns the
//! pre-write snapshot, not an error — benign, and self-correcting on the
//! next poll once `-wal` is seen to exist.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

/// Open `db_path` read-only — see the module doc for the stat-then-choose
/// strategy and what it costs.
pub(crate) fn open_read_only(db_path: &Path) -> rusqlite::Result<Connection> {
    if wal_sidecar_path(db_path).exists() {
        Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
    } else {
        let uri = format!(
            "file:{}?immutable=1",
            db_path.to_string_lossy().replace('\\', "/")
        );
        Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
    }
}

/// `<db_path>-wal`, the sidecar whose presence decides how [`open_read_only`]
/// opens the database. `pub(crate)`: also used directly by tests
/// constructing a specific sidecar-presence scenario.
pub(crate) fn wal_sidecar_path(db_path: &Path) -> PathBuf {
    let mut wal = db_path.to_path_buf();
    let name = wal
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    wal.set_file_name(format!("{name}-wal"));
    wal
}
