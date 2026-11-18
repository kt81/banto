//! One banto UI per database: an exclusive advisory lock on a file beside it.
//!
//! The lock is the operating system's (`File::try_lock`: `LockFileEx` on
//! Windows, `flock` on Unix), so it is released the instant the holder is gone,
//! however it went. Measured on Windows 11 (NTFS) and WSL2 (ext4) with rustc
//! 1.100 / 1.97: a holder that exits or is hard-killed (`taskkill /F`,
//! `kill -9`) frees it within about 24 ms (20 of 20 cycles), and a child that
//! outlives the holder does not keep it, because Rust opens files
//! non-inheritable (`O_CLOEXEC`) and a ConPTY child died with its parent. That
//! is why a lock file and not a PID file decides: a PID file left by a crash
//! needs a liveness guess, this needs none.
//!
//! The lock file itself is never deleted and nothing is read from or written to
//! it. Windows forbids other processes from reading the bytes of a locked file
//! (measured), so a payload in it would be unreadable exactly when it matters.
//! The holder's PID goes in a sidecar, `<lock file>.pid`, for the error message
//! only: it is written after the lock is taken, read best-effort when the lock
//! is found held, and never consulted for the decision. A stale or missing one
//! just means the message does not name a process.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// Why [`acquire`] did not hand back a lock.
#[derive(Debug)]
pub enum LockOutcome {
    /// Another banto holds it. `holder` is that process's PID when its sidecar
    /// could be read and parsed; it is a hint, not a fact.
    Held { holder: Option<u32> },
    /// Whether anyone holds it could not be found out: the lock file could not
    /// be opened, or the filesystem does not do locks (a network share, say). A
    /// caller that can still run decides whether that is fatal.
    Unchecked(io::Error),
}

impl std::fmt::Display for LockOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held { holder: Some(pid) } => {
                write!(f, "another banto (process {pid}) is using this database")
            }
            Self::Held { holder: None } => write!(f, "another banto is using this database"),
            Self::Unchecked(err) => write!(f, "could not check for another banto: {err}"),
        }
    }
}

impl std::error::Error for LockOutcome {}

/// The held lock. Dropping it releases it; so does the process ending.
#[derive(Debug)]
pub struct InstanceLock {
    /// Never read or written; open only to keep the lock alive.
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// The lock file this holds.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The lock file for a database: `<db file name>.lock` beside it.
///
/// The path is made canonical where it can be, so that two spellings of one
/// database (a relative path, a symlink, `..`, another drive-letter case) land
/// on one lock file and cannot both be held. An existing database is
/// canonicalised itself, which also follows a symlink to the real file. One
/// that does not exist yet has its directory canonicalised and its file name
/// appended; if even the directory is missing the path is used as it is, and
/// [`acquire`] will report that it cannot open the file.
pub fn lock_path(db_path: &Path) -> PathBuf {
    let base = db_path.canonicalize().unwrap_or_else(|_| {
        match (db_path.parent(), db_path.file_name()) {
            (Some(parent), Some(name)) => {
                // `Path::new("banto.db").parent()` is `Some("")`, which names
                // nothing; the current directory is what it means.
                let parent = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                parent
                    .canonicalize()
                    .map(|dir| dir.join(name))
                    .unwrap_or_else(|_| db_path.to_path_buf())
            }
            _ => db_path.to_path_buf(),
        }
    });
    with_suffix(base, ".lock")
}

fn with_suffix(path: PathBuf, suffix: &str) -> PathBuf {
    let mut name = path.into_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Take the exclusive lock for `db_path`, without waiting.
///
/// The lock file is created if it is not there, in a directory that already
/// exists (this makes none). Opened for reading *and* writing: an append-only
/// handle cannot be locked on Windows (measured).
pub fn acquire(db_path: &Path) -> Result<InstanceLock, LockOutcome> {
    let path = lock_path(db_path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(LockOutcome::Unchecked)?;
    match file.try_lock() {
        Ok(()) => {
            // Best effort: a message that cannot name its holder is still a
            // message, so a failure here is not worth refusing to start for.
            let _ = std::fs::write(sidecar_path(&path), std::process::id().to_string());
            Ok(InstanceLock { _file: file, path })
        }
        Err(TryLockError::WouldBlock) => Err(LockOutcome::Held {
            holder: read_holder(&path),
        }),
        Err(TryLockError::Error(err)) => Err(LockOutcome::Unchecked(err)),
    }
}

fn sidecar_path(lock_file: &Path) -> PathBuf {
    with_suffix(lock_file.to_path_buf(), ".pid")
}

fn read_holder(lock_file: &Path) -> Option<u32> {
    std::fs::read_to_string(sidecar_path(lock_file))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn names(path: &Path) -> String {
        path.file_name().unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn a_second_acquire_while_the_first_is_held_is_held_and_names_its_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let first = acquire(&db).unwrap();
        match acquire(&db).unwrap_err() {
            LockOutcome::Held { holder } => assert_eq!(holder, Some(std::process::id())),
            other => panic!("expected Held, got {other:?}"),
        }
        drop(first);
    }

    #[test]
    fn dropping_the_lock_releases_it_and_leaves_the_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let first = acquire(&db).unwrap();
        let lock_file = first.path().to_path_buf();
        drop(first);
        assert!(lock_file.exists(), "the lock file is never deleted");
        let again = acquire(&db).expect("released by the drop");
        assert_eq!(again.path(), lock_file);
    }

    #[test]
    fn two_spellings_of_one_database_share_one_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let _first = acquire(&db).unwrap();
        let dotted = dir.path().join("sub").join("..").join("banto.db");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert!(matches!(acquire(&dotted), Err(LockOutcome::Held { .. })));
    }

    #[test]
    fn databases_in_different_places_do_not_exclude_each_other() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let _held = acquire(&a.path().join("banto.db")).unwrap();
        assert!(acquire(&b.path().join("banto.db")).is_ok());
    }

    #[test]
    fn the_lock_file_sits_beside_an_existing_database_by_its_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        std::fs::write(&db, b"").unwrap();
        let expected = with_suffix(db.canonicalize().unwrap(), ".lock");
        assert_eq!(lock_path(&db), expected);
        // Reached by another spelling it is the same file.
        let dotted = dir.path().join(".").join("banto.db");
        assert_eq!(lock_path(&dotted), expected);
        assert_eq!(names(&expected), "banto.db.lock");
    }

    #[test]
    fn the_lock_file_for_a_database_not_yet_there_is_the_canonical_directory_plus_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sub").join("..").join("banto.db");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert!(!db.exists());
        let expected = with_suffix(dir.path().canonicalize().unwrap().join("banto.db"), ".lock");
        assert_eq!(lock_path(&db), expected);
        // Nothing was created by asking.
        assert!(!expected.exists());
    }

    #[test]
    fn a_database_in_a_missing_directory_keeps_its_spelling_and_cannot_be_checked() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("absent").join("banto.db");
        assert_eq!(lock_path(&db), with_suffix(db.clone(), ".lock"));
        assert!(matches!(acquire(&db), Err(LockOutcome::Unchecked(_))));
        assert!(!dir.path().join("absent").exists(), "no directory is made");
    }

    #[test]
    fn the_lock_file_is_open_for_reading_and_writing_and_stays_empty() {
        let dir = tempfile::tempdir().unwrap();
        let lock = acquire(&dir.path().join("banto.db")).unwrap();
        let mut file = &lock._file;
        // Append-only (or write-only) handles cannot be locked on Windows, so
        // the handle must read and write. Reading proves the one and resizing
        // to the length it already has proves the other, without a byte of
        // payload going in: the file carries none.
        assert_eq!(file.read(&mut [0u8; 1]).expect("readable"), 0);
        lock._file.set_len(0).expect("writable");
        assert_eq!(std::fs::metadata(lock.path()).unwrap().len(), 0);
    }

    /// Opening the second spelling opens the first one's file, so it finds the
    /// lock held even though the names differ and nothing is there yet.
    #[cfg(windows)]
    #[test]
    fn on_windows_two_spellings_that_differ_only_in_case_exclude_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(&dir.path().join("banto.db")).unwrap();
        match acquire(&dir.path().join("BANTO.DB")).unwrap_err() {
            LockOutcome::Held { holder } => assert_eq!(holder, Some(std::process::id())),
            other => panic!("expected Held, got {other:?}"),
        }
        drop(first);
    }

    #[test]
    fn the_sidecar_holds_the_pid_and_a_missing_or_garbled_one_names_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let lock = acquire(&db).unwrap();
        let sidecar = sidecar_path(lock.path());
        assert_eq!(names(&sidecar), "banto.db.lock.pid");
        assert_eq!(
            std::fs::read_to_string(&sidecar).unwrap(),
            std::process::id().to_string()
        );
        std::fs::write(&sidecar, "not a pid").unwrap();
        assert!(matches!(
            acquire(&db),
            Err(LockOutcome::Held { holder: None })
        ));
        std::fs::remove_file(&sidecar).unwrap();
        assert!(matches!(
            acquire(&db),
            Err(LockOutcome::Held { holder: None })
        ));
    }

    #[test]
    fn a_lock_file_that_cannot_be_opened_is_unchecked_not_held() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        // A directory where the file should be: opening it for writing fails on
        // every platform.
        std::fs::create_dir(with_suffix(
            dir.path().canonicalize().unwrap().join("banto.db"),
            ".lock",
        ))
        .unwrap();
        match acquire(&db).unwrap_err() {
            LockOutcome::Unchecked(_) => {}
            other => panic!("expected Unchecked, got {other:?}"),
        }
    }

    #[test]
    fn the_messages_say_what_is_known() {
        assert_eq!(
            LockOutcome::Held { holder: Some(42) }.to_string(),
            "another banto (process 42) is using this database"
        );
        assert_eq!(
            LockOutcome::Held { holder: None }.to_string(),
            "another banto is using this database"
        );
        assert!(
            LockOutcome::Unchecked(io::Error::other("no locks here"))
                .to_string()
                .contains("no locks here")
        );
    }
}
