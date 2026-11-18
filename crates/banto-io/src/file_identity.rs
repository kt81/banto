//! Which directory (or file) a path leads to, as the filesystem itself says it,
//! rather than by how the path is spelled.
//!
//! The question this answers is "are these two paths the same directory", for
//! the guard that keeps banto's files out of an agent's home, and "is this still
//! the file I resolved", for the banto executable a running UI hands to the
//! processes it starts. Spelling cannot answer the first: on Windows
//! `C:\Users\me\.claude` is also
//! `\\localhost\C$\Users\me\.claude`, `\\127.0.0.1\C$\…`, `\\?\UNC\localhost\C$\…`
//! and `\\<computer name>\C$\…`, and a short name (`CLAUDE~1`), a junction or a
//! symlink gets there too. There is no end to the spellings (a fully qualified
//! name, an IPv6 literal, a DNS alias, a mapped drive), so no list of them is
//! kept. Measured on Windows 11: `GetFileInformationByHandle` returns the same
//! volume serial number and file index for one directory through the plain path,
//! `\\?\`, `\\localhost\C$`, `\\127.0.0.1\C$`, `\\?\UNC\localhost\C$` and
//! `\\<computer name>\C$`.
//!
//! A third question, "is this reported path the directory a pane was started
//! in", is the one place a short list of spellings is kept instead
//! ([`DirectorySpellings`]): the paths to compare there are strings an agent or
//! its registry reported, and opening each to ask the filesystem can block on a
//! share that is gone.

use std::io;
use std::path::{Component, Path, Prefix};

/// What names one directory on one machine: the volume it is on and its number
/// there. Equal identities are the same directory by whatever path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    volume: u64,
    index: u128,
}

/// The identity of the directory `path` leads to, following symlinks and
/// junctions. An error when it does not exist or cannot be opened; callers that
/// ask about a path that may not be there yet treat that as "not this one".
pub fn directory_identity(path: &Path) -> io::Result<FileIdentity> {
    identity_of(path)
}

/// The identity of the file `path` leads to, following symlinks. The same probe
/// as [`directory_identity`], which opens nothing for reading, so it also works
/// on an executable that is running (measured on Windows 11 with the test
/// binary itself). A file replaced by a rename has a new identity, which is what
/// this exists to notice; one rewritten in place keeps its identity, and Unix
/// refuses to rewrite a running executable that way.
pub fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    identity_of(path)
}

/// The ways the directory banto spawned an agent in can be spelled, and whether
/// its volume folds letter case: what "is this path the directory I started
/// the agent in" is answered from, for the many paths an agent or its store
/// reports back.
///
/// The answer cannot be a `Path ==`. Measured on Windows 11 with the real
/// Claude Code 2.1.327 and Codex 0.169.0, one directory came back as: the
/// target of a symlink it was started through (Claude's live file and
/// transcript), the letter case it was typed in (the same), and
/// `\\?\` + the resolved path in the on-disk case (Codex's `threads.cwd`). On
/// macOS `/tmp` is itself a symlink to `/private/tmp`.
///
/// It is not answered by opening each reported path either, though that was
/// tried and is the more general answer ([`directory_identity`] sees through
/// every spelling). A reported path is a string banto does not control: one
/// `\\192.0.2.1\share\proj` that nothing answers froze a trust check for 21 s
/// (measured), and a stale mapped drive or dead NFS mount in a registry or a
/// transcript would do the same on every discovery pass, on the UI thread. So
/// only the target, which banto has just spawned in and so can reach, is ever
/// opened ([`Self::of`]); every candidate is compared as text
/// ([`Self::names`], no I/O at all).
///
/// A candidate matches when it is the typed spelling or the canonical one
/// (`std::fs::canonicalize`: symlinks and junctions resolved, the on-disk
/// case), after the `\\?\` and `\\?\UNC\` verbatim forms are written plainly
/// on both sides, compared ignoring letter case only on a volume that folds it.
/// What that cannot see, and the identity comparison could: a path through a
/// symlink that only part of the way is resolved, a bind mount or hard-linked
/// directory, a short (`PROGRA~1`) name. None has been seen in what an agent
/// reports. The `folds_case` probe opens the target and its re-cased spelling,
/// so it is as reachable as the target; a target on a share that is down stalls
/// here, as the spawn that preceded it would have.
#[derive(Debug, Clone)]
pub struct DirectorySpellings {
    spellings: Vec<Vec<String>>,
    folds_case: bool,
}

impl DirectorySpellings {
    /// The spellings of `target` and the case behaviour of its volume. The only
    /// I/O of this type: a canonicalisation and, to learn whether letter case
    /// folds, two directory opens, all on `target` itself. A target that does
    /// not exist keeps just its typed spelling and does not fold.
    pub fn of(target: &Path) -> Self {
        let mut spellings = vec![spelling_of(target)];
        if let Ok(canonical) = std::fs::canonicalize(target) {
            let canonical = spelling_of(&canonical);
            if !spellings.contains(&canonical) {
                spellings.push(canonical);
            }
        }
        Self {
            spellings,
            folds_case: volume_folds_case(target),
        }
    }

    /// The same from what was already measured, for the comparison's own tests.
    pub fn from_parts(spellings: &[&Path], folds_case: bool) -> Self {
        Self {
            spellings: spellings.iter().map(|path| spelling_of(path)).collect(),
            folds_case,
        }
    }

    /// Whether `candidate` is one of the spellings. Pure: it never touches the
    /// disk, whatever `candidate` is.
    pub fn names(&self, candidate: &Path) -> bool {
        let candidate = spelling_of(candidate);
        self.spellings.iter().any(|spelling| {
            spelling.len() == candidate.len()
                && spelling.iter().zip(&candidate).all(|(a, b)| {
                    a == b || (self.folds_case && a.to_lowercase() == b.to_lowercase())
                })
        })
    }
}

/// A path as its components, with the Windows verbatim prefixes written the way
/// everything else writes them: `\\?\C:\x` as `C:\x` and `\\?\UNC\s\x` as
/// `\\s\x`. Done on the text, so that the same text means the same thing on
/// every platform, and the components give a trailing separator, a repeated one
/// and (on Windows) `/` for `\` no say. A drive letter is written in capitals
/// whatever the volume does with the rest, as `Path ==` always treated it: `c:\x`
/// and `C:\x` are one path. Only a Windows path has a drive-letter component, so
/// elsewhere this changes nothing.
fn spelling_of(path: &Path) -> Vec<String> {
    let text = path.to_string_lossy();
    let plain = match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
    };
    Path::new(&plain)
        .components()
        .map(|part| {
            let text = part.as_os_str().to_string_lossy().into_owned();
            match part {
                Component::Prefix(prefix) if matches!(prefix.kind(), Prefix::Disk(_)) => {
                    text.to_ascii_uppercase()
                }
                _ => text,
            }
        })
        .collect()
}

/// Whether the volume `target` is on treats letter case as irrelevant: asked of
/// the volume, not guessed from the OS (NTFS and APFS usually do, ext4 does
/// not, and either can be configured the other way per directory). A variant
/// that equals the typed text proves nothing and is not tried.
fn volume_folds_case(target: &Path) -> bool {
    let Ok(own) = directory_identity(target) else {
        return false;
    };
    let text = target.to_string_lossy();
    [text.to_uppercase(), text.to_lowercase()]
        .into_iter()
        .filter(|variant| *variant != text)
        .any(|variant| directory_identity(Path::new(&variant)).is_ok_and(|other| other == own))
}

#[cfg(unix)]
fn identity_of(path: &Path) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    Ok(FileIdentity {
        volume: meta.dev(),
        index: u128::from(meta.ino()),
    })
}

/// A directory can only be opened with backup semantics, and with no access
/// rights at all: nothing is read from it, so this needs no permission on it
/// beyond being able to reach it. The flag changes nothing for a file.
#[cfg(windows)]
fn identity_of(path: &Path) -> io::Result<FileIdentity> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
    };

    let dir = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    // SAFETY: `dir` is an open handle for the whole call and the struct is
    // plain data that the call fills in.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(dir.as_raw_handle(), &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity {
        volume: u64::from(info.dwVolumeSerialNumber),
        index: (u128::from(info.nFileIndexHigh) << 32) | u128::from(info.nFileIndexLow),
    })
}

/// A platform with neither: nothing can be said, so nothing matches.
#[cfg(not(any(unix, windows)))]
fn identity_of(path: &Path) -> io::Result<FileIdentity> {
    let _ = path;
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// Fixtures for tests, here and in the modules that match directories by
/// identity, that need a second spelling of a real directory.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// Makes `link` a directory symlink to `target`. A machine that cannot
    /// (Windows without Developer Mode or elevation) fails the test, unless
    /// `BANTO_TEST_ALLOW_SKIPS=1` says a skip is wanted; then it says so and
    /// returns false.
    pub(crate) fn symlink_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(target, link);
        #[cfg(not(any(unix, windows)))]
        let made: std::io::Result<()> = Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
        match made {
            Ok(()) => true,
            Err(error) => {
                assert!(
                    std::env::var_os("BANTO_TEST_ALLOW_SKIPS").is_some_and(|v| v == "1"),
                    "cannot create a directory symlink here ({error}); set \
                     BANTO_TEST_ALLOW_SKIPS=1 to let that pass"
                );
                eprintln!("skipped: cannot create a directory symlink ({error})");
                false
            }
        }
    }

    /// `dir` spelled in another letter case, if this filesystem folds case: the
    /// upper-cased path is probed rather than assumed to work, since the answer
    /// belongs to the volume (APFS and NTFS usually fold, ext4 does not), not to
    /// the operating system.
    pub(crate) fn recased(dir: &Path) -> Option<PathBuf> {
        let upper = PathBuf::from(dir.to_string_lossy().to_ascii_uppercase());
        if upper == dir {
            return None;
        }
        let folds = match (
            super::directory_identity(dir),
            super::directory_identity(&upper),
        ) {
            (Ok(real), Ok(other)) => real == other,
            _ => false,
        };
        // NTFS folds case, so a failed probe on Windows is a broken probe, not a
        // volume to skip.
        assert!(
            folds || !cfg!(windows),
            "the temp volume should fold letter case on Windows, but {upper:?} is not {dir:?}"
        );
        if !folds {
            eprintln!("skipped: this filesystem does not fold letter case");
        }
        folds.then_some(upper)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_directory_reached_through_a_symlink_names_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(dir.path().join("other")).unwrap();
        if !test_support::symlink_dir(&real, &link) {
            return;
        }
        // Agents report the target of the symlink banto spawned through.
        let spawned = DirectorySpellings::of(&link);
        assert!(spawned.names(&link));
        assert!(spawned.names(&real.canonicalize().unwrap()));
        assert!(!spawned.names(&dir.path().join("other")));
    }

    #[test]
    fn a_directory_in_another_letter_case_names_itself_only_where_the_volume_folds_case() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("Real");
        std::fs::create_dir(&real).unwrap();
        let Some(recased) = test_support::recased(&real) else {
            // This volume keeps cases apart, and that is the answer to check.
            assert!(
                !DirectorySpellings::of(&real)
                    .names(&PathBuf::from(real.to_string_lossy().to_uppercase()))
            );
            return;
        };
        assert!(DirectorySpellings::of(&real).names(&recased));
        assert!(DirectorySpellings::of(&recased).names(&real));
    }

    /// Two directories whose names differ only in case, on a volume that allows
    /// it: the one spawned in must not be taken for the other.
    #[test]
    fn a_volume_that_keeps_cases_apart_does_not_fold_them() {
        let dir = tempfile::tempdir().unwrap();
        let lower = dir.path().join("proj");
        let upper = dir.path().join("PROJ");
        std::fs::create_dir(&lower).unwrap();
        if std::fs::create_dir(&upper).is_err() {
            eprintln!("skipped: this filesystem does not keep cases apart");
            return;
        }
        assert!(!DirectorySpellings::of(&lower).names(&upper));
        assert!(!DirectorySpellings::of(&upper).names(&lower));
    }

    /// The canonical form is the one Codex stores (`\\?\`-prefixed on Windows,
    /// `/private/tmp/…` on macOS) and the plain one is what banto spawned with.
    #[test]
    fn a_directory_and_its_canonical_form_name_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        assert!(DirectorySpellings::of(dir.path()).names(&canonical));
        assert!(DirectorySpellings::of(&canonical).names(dir.path()));
    }

    #[test]
    fn equal_spellings_match_without_a_directory_on_disk() {
        let absent = std::env::temp_dir().join("banto-no-such-dir-for-this-test");
        assert!(DirectorySpellings::of(&absent).names(&absent));
        assert!(!DirectorySpellings::of(&absent).names(&std::env::temp_dir()));
    }

    /// A relative path would be resolved against banto's own working
    /// directory; `.` must not read as the directory this test runs in.
    #[test]
    fn a_relative_path_does_not_name_the_working_directory() {
        let here = std::env::current_dir().unwrap();
        assert!(!DirectorySpellings::of(&here).names(Path::new(".")));
        assert!(!DirectorySpellings::of(&here).names(Path::new("")));
    }

    // The comparison itself, with no disk: what is measured is handed in.

    #[test]
    fn verbatim_prefixes_are_written_plainly_before_comparing() {
        let spawned = DirectorySpellings::from_parts(&[Path::new(r"C:\work\proj")], false);
        assert!(spawned.names(Path::new(r"\\?\C:\work\proj")));
        let unc = DirectorySpellings::from_parts(&[Path::new(r"\\srv\share\proj")], false);
        assert!(unc.names(Path::new(r"\\?\UNC\srv\share\proj")));
        assert!(!unc.names(Path::new(r"\\?\UNC\srv\share\other")));
        assert!(
            DirectorySpellings::from_parts(&[Path::new(r"\\?\UNC\srv\share\proj")], false)
                .names(Path::new(r"\\srv\share\proj"))
        );
    }

    #[test]
    fn either_given_spelling_matches_and_a_third_does_not() {
        let spawned = DirectorySpellings::from_parts(
            &[Path::new("/work/link"), Path::new("/data/real")],
            false,
        );
        assert!(spawned.names(Path::new("/work/link")));
        assert!(spawned.names(Path::new("/data/real")));
        assert!(!spawned.names(Path::new("/data/other")));
        assert!(!spawned.names(Path::new("/data")));
        assert!(!spawned.names(Path::new("/data/real/sub")));
    }

    #[test]
    fn letter_case_is_ignored_only_when_the_volume_folds_it() {
        let target = [Path::new("/work/Proj")];
        assert!(DirectorySpellings::from_parts(&target, true).names(Path::new("/WORK/proj")));
        assert!(!DirectorySpellings::from_parts(&target, false).names(Path::new("/WORK/proj")));
    }

    /// `Path ==` forgave the case of a drive letter, and a volume that keeps
    /// cases apart does not change that: the letter is not on the volume.
    #[cfg(windows)]
    #[test]
    fn a_drive_letter_is_matched_without_regard_to_case_on_a_case_keeping_volume() {
        let spawned = DirectorySpellings::from_parts(&[Path::new(r"c:\work\proj")], false);
        assert!(spawned.names(Path::new(r"C:\work\proj")));
        assert!(spawned.names(Path::new(r"\\?\C:\work\proj")));
        assert!(!spawned.names(Path::new(r"C:\WORK\proj")));
    }

    #[cfg(windows)]
    #[test]
    fn separators_and_a_trailing_one_make_no_difference_on_windows() {
        let spawned = DirectorySpellings::from_parts(&[Path::new(r"C:\work\proj")], false);
        assert!(spawned.names(Path::new("C:/work/proj")));
        assert!(spawned.names(Path::new(r"C:\work\proj\")));
    }

    /// A path nothing answers is just text: this is the case that froze a trust
    /// check for 21 s when each reported path was opened. The bound is far from
    /// the failure (instant against tens of seconds), so it is no timing race.
    #[test]
    fn a_path_on_a_share_that_never_answers_is_not_opened() {
        let started = std::time::Instant::now();
        let spawned = DirectorySpellings::from_parts(&[Path::new(r"C:\work\proj")], true);
        for candidate in [r"\\192.0.2.1\share\proj", r"\\?\UNC\192.0.2.1\share\proj"] {
            assert!(!spawned.names(Path::new(candidate)));
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn one_directory_has_one_identity_however_it_is_spelled() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a");
        std::fs::create_dir(&sub).unwrap();
        let direct = directory_identity(&sub).unwrap();
        assert_eq!(
            direct,
            directory_identity(&dir.path().join("a").join(".")).unwrap()
        );
        assert_eq!(
            direct,
            directory_identity(&dir.path().join("a").join("..").join("a")).unwrap()
        );
        assert_eq!(
            direct,
            directory_identity(&sub.canonicalize().unwrap()).unwrap()
        );
    }

    #[test]
    fn two_directories_have_two_identities() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        std::fs::create_dir(dir.path().join("b")).unwrap();
        assert_ne!(
            directory_identity(&dir.path().join("a")).unwrap(),
            directory_identity(&dir.path().join("b")).unwrap()
        );
        assert_ne!(
            directory_identity(&dir.path().join("a")).unwrap(),
            directory_identity(dir.path()).unwrap()
        );
    }

    #[test]
    fn one_file_has_one_identity_however_it_is_spelled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("banto-copy");
        std::fs::write(&file, b"x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let direct = file_identity(&file).unwrap();
        assert_eq!(
            direct,
            file_identity(&dir.path().join("sub").join("..").join("banto-copy")).unwrap()
        );
        assert_eq!(
            direct,
            file_identity(&file.canonicalize().unwrap()).unwrap()
        );
    }

    #[test]
    fn a_file_replaced_by_a_rename_has_another_identity() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("banto");
        let next = dir.path().join("banto.new");
        std::fs::write(&installed, b"old").unwrap();
        std::fs::write(&next, b"new").unwrap();
        let before = file_identity(&installed).unwrap();
        std::fs::rename(&next, &installed).unwrap();
        assert_ne!(before, file_identity(&installed).unwrap());
    }

    #[test]
    fn a_deleted_file_has_no_identity() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("banto");
        std::fs::write(&file, b"x").unwrap();
        file_identity(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert!(file_identity(&file).is_err());
    }

    /// The executable of a running process, the file the UI resolves at start.
    #[test]
    fn the_running_executable_has_an_identity() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(file_identity(&exe).unwrap(), file_identity(&exe).unwrap());
    }

    #[test]
    fn a_path_that_is_not_there_has_no_identity() {
        let dir = tempfile::tempdir().unwrap();
        assert!(directory_identity(&dir.path().join("absent")).is_err());
    }

    /// The reason the module exists. The admin share of the local machine is how
    /// a path gets a spelling no lexical rule predicts.
    #[cfg(windows)]
    #[test]
    fn on_windows_the_local_admin_share_leads_to_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().canonicalize().unwrap();
        let text = plain.to_string_lossy();
        // `\\?\C:\rest`
        let (drive, rest) = text
            .strip_prefix(r"\\?\")
            .and_then(|t| t.split_once(r":\"))
            .expect("a verbatim drive path");
        let direct = directory_identity(dir.path()).unwrap();
        for host in ["localhost", "127.0.0.1"] {
            let share = format!(r"\\{host}\{drive}$\{rest}");
            let Ok(through) = directory_identity(Path::new(&share)) else {
                assert!(
                    std::env::var_os("BANTO_TEST_ALLOW_SKIPS").is_some_and(|v| v == "1"),
                    "this machine has no admin share {share}; set BANTO_TEST_ALLOW_SKIPS=1 \
                     to let that pass"
                );
                eprintln!("skipped: no admin share {share}");
                continue;
            };
            assert_eq!(direct, through, "{share}");
        }
    }
}
