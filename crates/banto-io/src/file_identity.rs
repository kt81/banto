//! Which directory a path leads to, as the filesystem itself says it, rather
//! than by how the path is spelled.
//!
//! The one question this answers is "are these two paths the same directory",
//! for the guard that keeps banto's files out of an agent's home. Spelling
//! cannot answer it: on Windows `C:\Users\me\.claude` is also
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
#[cfg(unix)]
pub fn directory_identity(path: &Path) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    Ok(FileIdentity {
        volume: meta.dev(),
        index: u128::from(meta.ino()),
    })
}

/// The identity of the directory `path` leads to, following symlinks and
/// junctions. An error when it does not exist or cannot be opened; callers that
/// ask about a path that may not be there yet treat that as "not this one".
///
/// A directory can only be opened with backup semantics, and with no access
/// rights at all: nothing is read from it, so this needs no permission on it
/// beyond being able to reach it.
#[cfg(windows)]
pub fn directory_identity(path: &Path) -> io::Result<FileIdentity> {
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
pub fn directory_identity(path: &Path) -> io::Result<FileIdentity> {
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
