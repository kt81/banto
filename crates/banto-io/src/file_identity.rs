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

use std::io;
use std::path::Path;

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

#[cfg(test)]
mod tests {
    use super::*;

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
