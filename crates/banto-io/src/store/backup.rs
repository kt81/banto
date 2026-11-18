//! The copy of the database taken before a migration.
//!
//! A migration cannot be undone, and an older banto cannot open what a newer one
//! migrated, so a build that is about to upgrade a database keeps the database as
//! it was. The copy is `<database file>.before-v<N>` beside the database, where
//! the copy holds schema `N - 1` and `N` is the version the migration reaches
//! *first*: a v14 database is copied to `banto.db.before-v15` even when this
//! build goes on to v18. The name therefore always says what is inside, and a
//! build that knows schema `K` looks for `before-v<K + 1>`: the file an upgrade
//! past `K` left behind. The operator's hand-made `before-v15`, `-v16` and `-v17`
//! beside a v18 database read the same way (each is the database before it became
//! that version).
//!
//! A `.partial` file is the one thing banto deletes here: the next start (or a
//! failed attempt) removes its own working name. A finished copy is never
//! deleted, and not overwritten (see `publish` for the one filesystem case where
//! a race could).
//!
//! Only [`super::Store::open`], the UI's door, makes one. A brand-new or empty
//! database has nothing to keep, and the children of a running UI never migrate
//! (`Store::open_current`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::StoreError;
use super::migrations::SCHEMA_VERSION;

/// Where the copy of a database at schema `from` goes.
pub fn backup_path(db_path: &Path, from: i64) -> PathBuf {
    suffixed(db_path, &format!(".before-v{}", from + 1))
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map_or_else(OsString::new, OsString::from);
    name.push(suffix);
    path.with_file_name(name)
}

/// Copy the database `conn` is open on if it is about to be migrated.
///
/// Nothing to do for a database that is empty (version 0), already current, or
/// outside the range this build knows (the migration reports that one). A copy
/// that already exists is kept and no other is written: it is the older and so
/// the better one, and a start must not fail over it. Where the copy cannot be
/// written the migration must not run, so the error is the start's.
pub(super) fn before_migration(conn: &Connection, db_path: &Path) -> Result<(), StoreError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version <= 0 || version >= SCHEMA_VERSION {
        return Ok(());
    }
    let target = backup_path(db_path, version);
    let fail = |cause: &dyn std::fmt::Display| StoreError::Backup {
        path: target.clone(),
        cause: cause.to_string(),
    };
    match std::fs::exists(&target) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(err) => return Err(fail(&err)),
    }
    // Written under another name and renamed, so a copy cut short by a crash is
    // never taken for the backup. `VACUUM INTO` also refuses a target that exists,
    // which a crashed earlier attempt would leave.
    let partial = suffixed(&target, ".partial");
    match std::fs::remove_file(&partial) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(fail(&err)),
    }
    let written = (|| -> Result<(), String> {
        let text = partial
            .to_str()
            .ok_or_else(|| "the path is not valid UTF-8".to_string())?;
        conn.execute("VACUUM INTO ?1", [text])
            .map_err(|err| err.to_string())?;
        // `VACUUM INTO` does not say it has synced; a rename of unsynced bytes
        // could leave a named but empty backup after a power cut.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&partial)
            .and_then(|file| file.sync_all())
            .map_err(|err| err.to_string())?;
        publish(&partial, &target).map_err(|err| err.to_string())
    })();
    // A hard link leaves the partial name behind, and a failed copy or a lost
    // race for the name leaves it too.
    let _ = std::fs::remove_file(&partial);
    written.map_err(|cause| fail(&cause))
}

/// Give the finished copy its name, not replacing a file that has it.
///
/// A hard link fails when the name is taken, on NTFS and on Unix alike, where
/// `rename` would silently replace the destination on Windows: so a backup that
/// appeared after `before_migration` looked (another banto, the operator) stays
/// what it was. The partial name is left for the caller to remove. A filesystem
/// with no hard links (FAT, some network shares) cannot do this, and there a
/// look-then-rename is all that is left: it is not atomic, so a copy that
/// appears between the look and the rename is replaced.
fn publish(partial: &Path, target: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(partial, target) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => {
            if std::fs::exists(target)? {
                return Ok(());
            }
            std::fs::rename(partial, target)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::migrations::MIGRATIONS;
    use super::super::{Store, StoreError};
    use super::*;

    /// A database file as a build at schema `version` left it.
    fn database_at(dir: &Path, version: usize) -> PathBuf {
        let db = dir.join("banto.db");
        let conn = Connection::open(&db).unwrap();
        for script in &MIGRATIONS[0..version] {
            conn.execute_batch(script).unwrap();
        }
        conn.pragma_update(None, "user_version", version as i64)
            .unwrap();
        db
    }

    fn user_version(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_name_says_the_version_the_migration_reaches_first() {
        assert_eq!(
            backup_path(Path::new("data/banto.db"), 14),
            Path::new("data/banto.db.before-v15")
        );
    }

    #[test]
    fn an_upgrade_leaves_a_copy_at_the_old_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = database_at(dir.path(), 14);

        drop(Store::open(&db).unwrap());

        assert_eq!(user_version(&db), SCHEMA_VERSION);
        assert_eq!(user_version(&dir.path().join("banto.db.before-v15")), 14);
        assert_eq!(names(dir.path()), ["banto.db", "banto.db.before-v15"]);
    }

    #[test]
    fn the_copy_holds_what_the_database_held() {
        let dir = tempfile::tempdir().unwrap();
        let db = database_at(dir.path(), 14);
        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO pins (session_id, pinned_at_ms) VALUES ('s1', 1)",
                [],
            )
            .unwrap();

        drop(Store::open(&db).unwrap());

        let copy = Connection::open(dir.path().join("banto.db.before-v15")).unwrap();
        let pins: i64 = copy
            .query_row("SELECT count(*) FROM pins", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pins, 1);
    }

    #[test]
    fn a_new_database_is_not_copied() {
        let dir = tempfile::tempdir().unwrap();
        drop(Store::open(&dir.path().join("banto.db")).unwrap());
        assert_eq!(names(dir.path()), ["banto.db"]);
    }

    #[test]
    fn an_empty_file_is_not_copied() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        std::fs::write(&db, b"").unwrap();
        drop(Store::open(&db).unwrap());
        assert_eq!(names(dir.path()), ["banto.db"]);
    }

    #[test]
    fn a_current_database_is_not_copied() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        drop(Store::open(&db).unwrap());
        drop(Store::open(&db).unwrap());
        assert_eq!(names(dir.path()), ["banto.db"]);
    }

    #[test]
    fn an_existing_copy_is_kept_and_the_start_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let db = database_at(dir.path(), 14);
        let backup = dir.path().join("banto.db.before-v15");
        std::fs::write(&backup, b"the operator's own").unwrap();

        drop(Store::open(&db).unwrap());

        assert_eq!(std::fs::read(&backup).unwrap(), b"the operator's own");
        assert_eq!(user_version(&db), SCHEMA_VERSION);
        assert_eq!(names(dir.path()), ["banto.db", "banto.db.before-v15"]);
    }

    #[test]
    fn a_partial_copy_left_by_a_crash_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let db = database_at(dir.path(), 14);
        std::fs::write(dir.path().join("banto.db.before-v15.partial"), b"cut short").unwrap();

        drop(Store::open(&db).unwrap());

        assert_eq!(user_version(&dir.path().join("banto.db.before-v15")), 14);
        assert_eq!(names(dir.path()), ["banto.db", "banto.db.before-v15"]);
    }

    #[test]
    fn a_copy_that_cannot_be_written_stops_the_start_and_leaves_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = database_at(dir.path(), 14);
        // A directory with something in it where the working name goes.
        let partial = dir.path().join("banto.db.before-v15.partial");
        std::fs::create_dir(&partial).unwrap();
        std::fs::write(partial.join("x"), b"").unwrap();

        let Err(err) = Store::open(&db) else {
            panic!("no copy, no migration");
        };

        assert!(matches!(err, StoreError::Backup { .. }), "{err:?}");
        let message = err.to_string();
        let named = dir.path().join("banto.db.before-v15");
        assert!(message.contains(&named.display().to_string()), "{message}");
        assert!(message.contains("not migrated"), "{message}");
        assert!(!message.contains("  "), "a stray run of spaces: {message}");
        assert_eq!(user_version(&db), 14, "the database is untouched");
    }

    /// The copy that appears between the existence check and the final step.
    #[test]
    fn publishing_never_replaces_a_copy_that_is_already_there() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("banto.db.before-v15.partial");
        let target = dir.path().join("banto.db.before-v15");
        std::fs::write(&partial, b"ours").unwrap();
        std::fs::write(&target, b"theirs").unwrap();

        publish(&partial, &target).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"theirs");
    }

    #[test]
    fn publishing_gives_the_copy_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("banto.db.before-v15.partial");
        let target = dir.path().join("banto.db.before-v15");
        std::fs::write(&partial, b"ours").unwrap();

        publish(&partial, &target).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"ours");
    }

    #[test]
    fn a_newer_database_is_refused_with_the_fix_and_is_not_copied() {
        let dir = tempfile::tempdir().unwrap();
        let db = database_at(dir.path(), 14);
        Connection::open(&db)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 2)
            .unwrap();

        let Err(err) = Store::open(&db) else {
            panic!("a newer schema must be refused");
        };

        let message = err.to_string();
        assert!(message.contains("newer banto"), "{message}");
        assert!(message.contains("banto.db.before-v<N>"), "{message}");
        assert!(
            message.contains(&format!("N at most {}", SCHEMA_VERSION + 1)),
            "{message}"
        );
        assert!(!message.contains('\n'), "one line: {message}");
        assert!(!message.contains("  "), "a stray run of spaces: {message}");
        assert_eq!(names(dir.path()), ["banto.db"]);
    }
}
