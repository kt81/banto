//! Reading the model catalog compiled into the installed Codex binary, so a
//! family word (`terra`) can become a slug at the moment a session is created
//! (`banto_core::codex_model`, which holds the decision; this holds the process).
//!
//! `codex debug models --bundled` prints that catalog as one line of JSON. It
//! was run by another member of this brigade (not by this module's tests): no
//! network, no auth, no dependence on the Codex home's config, about 0.1 s, exit
//! 0. It is a visible debug subcommand whose output tracks Codex's internal
//! struct, which is why the reading is lenient and why a failure of any kind is
//! reported as a plain reason, never a panic — the caller then does what the
//! rules for an unreadable catalog say.
//!
//! Behind a trait for the reason every external process is
//! (`docs/DISCIPLINE.md`): tests of the emporium never run Codex. The only test
//! of the real process here runs a stand-in program, never `codex`.
//!
//! Nothing is cached. The catalog belongs to a binary, and the binary replaces
//! itself when Codex updates; a copy kept from before would name models the new
//! one has dropped. The price is one 0.1 s process per creation that needs it,
//! which the creating Workers of one brigade share (the engine asks once and
//! answers every Worker waiting on it).

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long the command may run before it is given up on. It takes about a
/// tenth of a second, so this is generous for a cold start under a virus
/// scanner and short enough that a hung binary costs the operator one pause and
/// not the session: the call is made on the event loop's thread, like a PTY
/// spawn, so the bound is also how long the screen can stand still.
pub const CATALOG_TIMEOUT: Duration = Duration::from_secs(5);

/// More than this on standard output is not a catalog; stop reading it.
const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;

/// What can say which models the installed Codex ships with.
pub trait CodexCatalogSource {
    /// The catalog as the command printed it (standard output only), or why it
    /// could not be had: the binary is missing, it exited non-zero, it timed
    /// out, or its output could not be read. The text is shown to the operator.
    fn bundled_catalog(&self, binary: &str) -> Result<String, String>;
}

/// Runs `<binary> debug models --bundled`.
pub struct CodexCli;

impl CodexCatalogSource for CodexCli {
    fn bundled_catalog(&self, binary: &str) -> Result<String, String> {
        let program = resolve_program(
            binary,
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("PATHEXT").as_deref(),
        );
        run_for_stdout(&program, &["debug", "models", "--bundled"], CATALOG_TIMEOUT)
    }
}

/// Find `name` the way the PTY host that will launch it does: a path as it is,
/// a bare name on `PATH`, and on Windows each `PATHEXT` extension in turn — a
/// Codex installed by npm is `codex.cmd`, which a bare `Command::new("codex")`
/// would not find (it only adds `.exe`). Nothing found returns the name
/// unchanged, so the spawn fails with its own message.
fn resolve_program(name: &str, path: Option<&OsStr>, pathext: Option<&OsStr>) -> PathBuf {
    let given = Path::new(name);
    if given.components().count() > 1 || given.is_absolute() {
        return given.to_path_buf();
    }
    let extensions: Vec<String> = pathext
        .map(|raw| {
            raw.to_string_lossy()
                .split(';')
                .filter(|ext| !ext.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for dir in path.iter().flat_map(|p| std::env::split_paths(p)) {
        let bare = dir.join(name);
        if extensions.is_empty() {
            if bare.is_file() {
                return bare;
            }
            continue;
        }
        // An explicit extension already on the name is taken as it is first.
        if given.extension().is_some() && bare.is_file() {
            return bare;
        }
        for ext in &extensions {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    given.to_path_buf()
}

/// Run `program args…` with standard input closed and standard error dropped
/// (a warning there is not part of the answer), and return its standard output
/// if it exits 0 within `timeout`.
fn run_for_stdout(program: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("could not run {}: {err}", program.display()))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    // Read on a thread: a child that fills the pipe would otherwise block on
    // its write while this loop waits for it to exit.
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.take(MAX_OUTPUT_BYTES).read_to_end(&mut text);
        text
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", timeout.as_secs().max(1)));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => return Err(format!("could not wait for it: {err}")),
        }
    };
    let bytes = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(match status.code() {
            Some(code) => format!("exit status {code}"),
            None => "ended by a signal".to_string(),
        });
    }
    String::from_utf8(bytes).map_err(|_| "output was not text".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program that prints `text` and exits `code`, and one that outlives any
    /// timeout: stand-ins for Codex, written for the platform the tests run on.
    #[cfg(windows)]
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("{name}.cmd"));
        std::fs::write(&path, format!("@echo off\r\n{body}\r\n")).unwrap();
        path
    }

    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(windows)]
    const SLEEP_A_WHILE: &str = "ping -n 8 127.0.0.1 > nul";
    #[cfg(unix)]
    const SLEEP_A_WHILE: &str = "sleep 8";

    #[test]
    fn a_program_that_exits_zero_gives_its_standard_output_and_not_its_standard_error() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "fake", "echo catalog-line\necho warning 1>&2");
        let out = run_for_stdout(&program, &["debug"], Duration::from_secs(20)).unwrap();
        assert!(out.contains("catalog-line"), "{out:?}");
        assert!(!out.contains("warning"), "{out:?}");
    }

    #[test]
    fn a_non_zero_exit_is_an_error_naming_the_status() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "fake", "exit 3");
        let err = run_for_stdout(&program, &[], Duration::from_secs(20)).unwrap_err();
        assert_eq!(err, "exit status 3");
    }

    #[test]
    fn a_program_that_does_not_exist_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_for_stdout(
            &dir.path().join("nothing-here"),
            &[],
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(err.starts_with("could not run "), "{err}");
    }

    #[test]
    fn a_program_that_hangs_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "fake", SLEEP_A_WHILE);
        let started = Instant::now();
        let err = run_for_stdout(&program, &[], Duration::from_millis(300)).unwrap_err();
        assert!(err.starts_with("timed out after"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "gave up at the timeout, not when the program ended"
        );
    }

    #[test]
    fn a_bare_name_is_found_on_path_and_a_path_is_taken_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        let file = dir.path().join("fakecodex");
        std::fs::write(&file, "").unwrap();
        assert_eq!(resolve_program("fakecodex", Some(&path_var), None), file);
        // Not on PATH: the name comes back unchanged for the spawn to report.
        assert_eq!(
            resolve_program("absent", Some(&path_var), None),
            PathBuf::from("absent")
        );
        // A path is not searched for.
        let explicit = dir.path().join("elsewhere").join("codex");
        assert_eq!(
            resolve_program(explicit.to_str().unwrap(), Some(&path_var), None),
            explicit
        );
    }

    #[test]
    fn pathext_extensions_are_tried_in_order_so_an_npm_shim_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        std::fs::write(dir.path().join("fakecodex.cmd"), "").unwrap();
        // The extension later in the list is the only one present: found.
        let pathext = OsStr::new(".com;.exe;.cmd");
        assert_eq!(
            resolve_program("fakecodex", Some(&path_var), Some(pathext)),
            dir.path().join("fakecodex.cmd")
        );
        // With an earlier one present as well, the earlier wins.
        std::fs::write(dir.path().join("fakecodex.exe"), "").unwrap();
        assert_eq!(
            resolve_program("fakecodex", Some(&path_var), Some(pathext)),
            dir.path().join("fakecodex.exe")
        );
    }
}
