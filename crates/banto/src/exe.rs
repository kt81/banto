//! The banto executable, resolved once when the UI starts.
//!
//! Every banto process a member's agent starts is this executable again: the
//! `_mcp` server named in a Claude member's `--mcp-config`, the same server in
//! a Codex member's `-c` overrides, and the `_hook` command Codex runs at
//! `SessionStart`. Asking `std::env::current_exe()` at each of those places was
//! asking the operating system, at that moment, what the running image is
//! called — and after an upgrade the answer changes underneath a running UI.
//! Measured on Linux (WSL): once a binary is replaced by a rename,
//! `/proc/<pid>/exe` reads `<path> (deleted)`, a path with a space in it, which
//! the Codex hook command cannot carry (`launch::hook_command_is_launchable`)
//! and so would drop the hook with no sign of it.
//!
//! So the path is read once, together with the identity of the file it leads to
//! (`banto_io::file_identity`), and handed down like `startup::Resolved`. At each
//! member open the identity is read again: a file that is gone or is another
//! file means a banto that is not the one running, whose children would be a
//! different build from this UI (`Store::open_current` refuses those anyway, but
//! only after the agent has started), so the open is refused instead.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use banto_io::file_identity::{FileIdentity, file_identity};

use crate::launch;

/// What a refused open says. One line, the status channel's.
pub const REPLACED: &str = "banto was replaced on disk; restart banto";

/// The executable this UI is running from, as it was when the UI started.
#[derive(Debug, Clone)]
pub enum BantoExe {
    /// `identity` is the error kind where the file could not be probed at start:
    /// nothing can then be noticed, and [`BantoExe::watch_notice`] says so.
    Known {
        path: PathBuf,
        identity: Result<FileIdentity, ErrorKind>,
    },
    /// The system would not name the executable; the reason, for the status line
    /// of any open that needs it.
    Unknown(String),
}

impl BantoExe {
    /// The running executable's path and identity, read now. Never fails: a
    /// banto that cannot name itself still lists, searches and opens sessions
    /// that are not in a brigade.
    pub fn resolve() -> BantoExe {
        match std::env::current_exe() {
            Ok(path) => BantoExe::at(path),
            Err(err) => {
                BantoExe::Unknown(format!("cannot determine banto's own executable: {err}"))
            }
        }
    }

    /// `path` as the executable, with the identity it has now.
    pub fn at(path: PathBuf) -> BantoExe {
        let identity = file_identity(&path).map_err(|err| err.kind());
        BantoExe::Known { path, identity }
    }

    /// The path resolved at start, without looking at the file again: for what
    /// only needs to know how the executable is spelled (whether a hook command
    /// can carry it).
    pub fn path(&self) -> Option<&Path> {
        match self {
            BantoExe::Known { path, .. } => Some(path),
            BantoExe::Unknown(_) => None,
        }
    }

    /// Whether the Codex hook command could carry the executable's path (no space
    /// in it), by the path resolved at start. An unknowable path counts as
    /// launchable: blaming a space nothing has established is there would be worse
    /// than leaving the trust question to speak for itself.
    pub fn hook_launchable(&self) -> bool {
        self.path()
            .is_none_or(|exe| launch::hook_command_is_launchable(&launch::forward_slash_path(exe)))
    }

    /// What the operator is told at start when a replacement of the executable
    /// cannot be noticed this run: without the identity at start, [`check`]
    /// passes whatever happens to the file.
    ///
    /// [`check`]: BantoExe::check
    pub fn watch_notice(&self) -> Option<String> {
        match self {
            BantoExe::Known {
                identity: Err(kind),
                ..
            } => Some(format!(
                "banto cannot tell whether its executable is replaced on disk this run \
                 (reading the file's identity failed: {kind})"
            )),
            _ => None,
        }
    }

    /// The path to hand to a banto child that is about to be started, or why not.
    /// Reads the file's identity again: a file that is gone, or is a different
    /// file from the one resolved at start, is [`REPLACED`].
    pub fn check(&self) -> Result<&Path, String> {
        match self {
            BantoExe::Unknown(reason) => Err(reason.clone()),
            BantoExe::Known {
                path,
                identity: Err(_),
            } => Ok(path),
            BantoExe::Known {
                path,
                identity: Ok(at_start),
            } => match file_identity(path) {
                Ok(now) if now == *at_start => Ok(path),
                // Gone, unreadable or another file: the one this UI runs from is
                // not what a child would be started from.
                _ => Err(REPLACED.to_string()),
            },
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::BantoExe;
    use std::sync::OnceLock;

    /// The test binary itself, resolved once like a real run does.
    pub(crate) fn current() -> &'static BantoExe {
        static EXE: OnceLock<BantoExe> = OnceLock::new();
        EXE.get_or_init(BantoExe::resolve)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_running_executable_resolves_and_passes_its_own_check() {
        let exe = BantoExe::resolve();
        let path = exe.check().expect("the test binary is where it was");
        assert_eq!(Some(path), exe.path());
        assert!(matches!(
            exe,
            BantoExe::Known {
                identity: Ok(_),
                ..
            }
        ));
    }

    #[test]
    fn a_file_left_alone_passes_every_check() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("banto");
        std::fs::write(&file, b"v1").unwrap();
        let exe = BantoExe::at(file.clone());
        assert_eq!(exe.check().unwrap(), file);
        assert_eq!(exe.check().unwrap(), file);
    }

    /// An upgrade: a new build renamed into the path.
    #[test]
    fn a_file_replaced_by_a_rename_is_refused_in_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("banto");
        std::fs::write(&file, b"v1").unwrap();
        let exe = BantoExe::at(file.clone());
        let next = dir.path().join("banto.new");
        std::fs::write(&next, b"v2").unwrap();
        std::fs::rename(&next, &file).unwrap();

        let refusal = exe.check().unwrap_err();

        assert_eq!(refusal, "banto was replaced on disk; restart banto");
        assert!(!refusal.contains('\n'));
        // The path itself is still the one resolved at start.
        assert_eq!(exe.path(), Some(file.as_path()));
    }

    #[test]
    fn a_file_that_is_gone_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("banto");
        std::fs::write(&file, b"v1").unwrap();
        let exe = BantoExe::at(file.clone());
        std::fs::remove_file(&file).unwrap();
        assert_eq!(exe.check().unwrap_err(), REPLACED);
    }

    /// Where the file could not be probed at start there is nothing to compare:
    /// the start goes on, and says so once.
    #[test]
    fn a_path_with_no_file_at_start_is_not_refused_but_the_operator_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("never-there");
        let exe = BantoExe::at(file.clone());
        assert!(matches!(
            exe,
            BantoExe::Known {
                identity: Err(ErrorKind::NotFound),
                ..
            }
        ));
        assert_eq!(exe.check().unwrap(), file);
        let notice = exe.watch_notice().expect("a notice");
        assert!(notice.contains("cannot tell"), "{notice}");
        assert!(notice.contains("not found"), "{notice}");
        assert!(!notice.contains('\n') && !notice.contains("  "), "{notice}");
    }

    #[test]
    fn a_watched_executable_has_no_notice() {
        assert_eq!(BantoExe::resolve().watch_notice(), None);
        let unknown = BantoExe::Unknown("boom".to_string());
        assert_eq!(unknown.watch_notice(), None);
    }

    #[test]
    fn an_executable_the_system_would_not_name_is_refused_with_the_reason() {
        let exe = BantoExe::Unknown("cannot determine banto's own executable: boom".to_string());
        assert_eq!(exe.path(), None);
        assert!(exe.check().unwrap_err().contains("boom"));
    }

    /// The Linux case this exists for: the path stays what it was at start, with
    /// no ` (deleted)` in it, however the file underneath changes.
    #[test]
    fn the_resolved_path_does_not_change_when_the_file_does() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("banto");
        std::fs::write(&file, b"v1").unwrap();
        let exe = BantoExe::at(file.clone());
        std::fs::remove_file(&file).unwrap();
        assert_eq!(exe.path(), Some(file.as_path()));
        assert!(!exe.path().unwrap().to_string_lossy().contains("(deleted)"));
    }
}
