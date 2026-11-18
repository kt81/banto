//! One banto UI per database: the claim `main` makes before it opens the store.
//!
//! Only the UI claims. `_mcp` and `_hook` are children of a running UI (or, for
//! the hook, of an agent a UI started) and open the very database the UI holds;
//! `list` and `versions` open none. The lock itself, and the measurements it
//! rests on, are `banto_io::instance_lock`.
//!
//! The claim comes before `Store::open` because opening migrates: a newer banto
//! migrating a database an older banto is running on would leave that one unable
//! to reopen its own database.

use std::path::Path;

use anyhow::{Result, bail};
use banto_io::instance_lock::{self, InstanceLock, LockOutcome};

/// What the claim left behind: the lock to hold until the UI is gone, and a
/// status-line notice when the check could not be made.
#[derive(Debug)]
pub struct Claim {
    /// `None` only when the check could not be made.
    pub lock: Option<InstanceLock>,
    pub notice: Option<String>,
}

/// Take the right to use `db_path` without waiting.
///
/// A database held by another banto is an error: the operator decided that a UI
/// that would share a database with a running one must not start. A check that
/// cannot be made is not, and the notice says it could not look: the lock file
/// cannot be opened, or locking itself fails (a filesystem with no locks).
/// Failing there would keep banto off a share that merely lacks locks, for no
/// protection gained.
///
/// The database's directory is created first, so that a first run takes the lock
/// too. When that cannot be done the claim is likewise unchecked, but the UI does
/// not start on that notice: `Store::open` meets the same directory next and
/// fails, as it did before the lock existed.
pub fn claim_database(db_path: &Path) -> Result<Claim> {
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return Ok(unchecked(&err));
    }
    match instance_lock::acquire(db_path) {
        Ok(lock) => Ok(Claim {
            lock: Some(lock),
            notice: None,
        }),
        Err(LockOutcome::Held { holder }) => {
            let by = holder.map_or_else(String::new, |pid| format!(" (process {pid})"));
            bail!(
                "another banto{by} is already using the database {}: quit that banto, or start \
                 this one on another database with BANTO_DATA_DIR, or the config key `data_dir` \
                 or `db_path`",
                db_path.display()
            )
        }
        Err(LockOutcome::Unchecked(err)) => Ok(unchecked(&err)),
    }
}

fn unchecked(err: &std::io::Error) -> Claim {
    Claim {
        lock: None,
        notice: Some(format!(
            "single-instance check failed ({:?}): a second banto on this database would not be \
             refused",
            err.kind()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_io::store::Store;

    #[test]
    fn a_free_database_is_claimed_without_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let claim = claim_database(&db).unwrap();
        assert!(claim.lock.is_some());
        assert!(claim.notice.is_none());
    }

    #[test]
    fn a_database_another_banto_holds_is_refused_by_name_and_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let _first = claim_database(&db).unwrap();
        let message = format!("{:#}", claim_database(&db).unwrap_err());
        assert!(message.contains(&db.display().to_string()), "{message}");
        assert!(
            message.contains(&format!("process {}", std::process::id())),
            "{message}"
        );
        for way in ["BANTO_DATA_DIR", "data_dir", "db_path"] {
            assert!(message.contains(way), "{way}: {message}");
        }
        assert!(!message.contains("--"), "a hidden flag is named: {message}");
        assert!(!message.contains('\n'), "one line: {message}");
        assert!(!message.contains("  "), "a stray run of spaces: {message}");
    }

    #[test]
    fn the_claim_is_free_again_once_the_holder_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        drop(claim_database(&db).unwrap());
        assert!(claim_database(&db).unwrap().lock.is_some());
    }

    #[test]
    fn a_first_run_creates_the_directory_and_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("not").join("yet").join("banto.db");
        let first = claim_database(&db).unwrap();
        assert!(first.lock.is_some() && first.notice.is_none());
        assert!(db.parent().unwrap().is_dir());
        assert!(
            claim_database(&db).is_err(),
            "the lock taken on a first run"
        );
    }

    #[test]
    fn a_lock_file_that_cannot_be_opened_starts_with_a_notice_naming_the_kind() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        // A directory where the lock file should be: opening it for writing fails.
        std::fs::create_dir(instance_lock::lock_path(&db)).unwrap();
        let claim = claim_database(&db).unwrap();
        assert!(claim.lock.is_none());
        let notice = claim.notice.expect("a notice");
        assert!(notice.contains("check failed"), "{notice}");
        assert!(notice.len() < 120, "short: {notice}");
    }

    #[test]
    fn a_directory_that_cannot_be_made_makes_the_claim_unchecked() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("a-file");
        std::fs::write(&blocker, b"").unwrap();
        let claim = claim_database(&blocker.join("sub").join("banto.db")).unwrap();
        assert!(claim.lock.is_none());
        assert!(claim.notice.unwrap().contains("check failed"));
    }

    /// The hook and the MCP server never claim, so a running UI does not stop
    /// them opening the store it holds.
    #[test]
    fn a_held_claim_does_not_stop_the_store_opening() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let _ui = claim_database(&db).unwrap();
        Store::open(&db).expect("the ui's own children open it");
    }
}
