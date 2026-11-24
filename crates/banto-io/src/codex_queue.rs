//! Running `codex queue --thread <id> --message <text>`, which appends a message
//! to a Codex session's queue without typing it into the pane
//! (`banto_core::codex_queue` holds the arguments and reads what it printed).
//!
//! Behind a trait for the reason every external process is
//! (`docs/DISCIPLINE.md`): tests of the emporium never run Codex. The only test
//! of the real process here runs a stand-in program, never `codex`.
//!
//! The child inherits banto's environment and sets nothing of its own. In
//! particular it does not set `CODEX_HOME`: a member's pane is launched without
//! one either (`banto`'s `pane_env`), so both read `$CODEX_HOME` as banto saw it
//! (else `~/.codex`), and the queue lands in the home the member's thread lives
//! in. That holds only while neither `--codex-home` nor the config's `codex_home`
//! points banto elsewhere. When one does and differs from that environment
//! value, banto lists, probes and reads rollouts in its own home while the pane
//! and this process use the other, so a queued message is not found where banto
//! looks for it.
//!
//! One process takes about 0.2 s, measured on Windows with Codex 0.171.0. It is
//! run on the event loop's thread, like the catalog read, so the timeout is also
//! how long the screen can stand still.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use banto_core::codex_queue::{QueueRun, queue_args};

use crate::codex_catalog::resolve_program;

/// How long `codex queue` may run before it is given up on and killed. A kill
/// leaves it unknown whether the message was queued.
pub const QUEUE_TIMEOUT: Duration = Duration::from_secs(5);

/// More than this on either stream is not an answer; stop reading it.
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;

/// What can put a message on a Codex session's queue.
pub trait CodexQueueSender {
    /// Runs `<binary> queue --thread <thread_id> --message <message>` and returns
    /// what it did, or why it could not be started at all (the binary is
    /// missing, or the system refused the arguments).
    fn queue(&self, binary: &str, thread_id: &str, message: &str) -> Result<QueueRun, String>;
}

/// Runs the real `codex`.
pub struct CodexQueueCli;

impl CodexQueueSender for CodexQueueCli {
    fn queue(&self, binary: &str, thread_id: &str, message: &str) -> Result<QueueRun, String> {
        let program = resolve_program(
            binary,
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("PATHEXT").as_deref(),
        );
        run_captured(&program, &queue_args(thread_id, message), QUEUE_TIMEOUT)
    }
}

/// Run `program args…` with standard input closed and both output streams
/// captured, and return how it ended.
///
/// An `Err` is only a process that never ran. A non-zero exit, a signal and a
/// timeout are all a [`QueueRun`], because what the caller does with them
/// differs and the reading is the pure half's.
fn run_captured(program: &Path, args: &[String], timeout: Duration) -> Result<QueueRun, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("could not run {}: {err}", program.display()))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    // Read each on its own thread: a child that fills one pipe would otherwise
    // block on its write while this loop waits for it to exit.
    let read_all = |stream: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stream.take(MAX_OUTPUT_BYTES).read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let stdout = read_all(Box::new(stdout));
    let stderr = read_all(Box::new(stderr));
    let started = Instant::now();
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                // The readers are left to end on their own and nothing they
                // read is kept. Killing a `.cmd` shim leaves the program it
                // started holding the pipes, so joining them would wait for
                // that program and not for the timeout.
                return Ok(QueueRun {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    timed_out: true,
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => return Err(format!("could not wait for it: {err}")),
        }
    };
    Ok(QueueRun {
        exit_code,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
        timed_out: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a stand-in for Codex whose behaviour is `body`, in the shell script
    /// language of the platform the tests run on. See `codex_catalog`'s tests for
    /// why Unix runs `sh -c` and does not write an executable first.
    #[cfg(windows)]
    fn run_stand_in(body: &str, timeout: Duration) -> Result<QueueRun, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.cmd");
        std::fs::write(
            &path,
            format!(
                "@echo off
{body}
"
            ),
        )
        .unwrap();
        run_captured(&path, &["queue".to_string()], timeout)
    }

    #[cfg(unix)]
    fn run_stand_in(body: &str, timeout: Duration) -> Result<QueueRun, String> {
        run_captured(
            Path::new("/bin/sh"),
            &["-c".to_string(), body.to_string(), "queue".to_string()],
            timeout,
        )
    }

    #[cfg(windows)]
    const SLEEP_A_WHILE: &str = "ping -n 8 127.0.0.1 > nul";
    #[cfg(unix)]
    const SLEEP_A_WHILE: &str = "exec sleep 8";

    #[test]
    fn both_streams_and_the_exit_code_of_a_failing_program_are_kept() {
        let run = run_stand_in(
            "echo said-on-stdout
echo said-on-stderr 1>&2
exit 4",
            Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(run.exit_code, Some(4));
        assert!(run.stdout.contains("said-on-stdout"), "{run:?}");
        assert!(run.stderr.contains("said-on-stderr"), "{run:?}");
        assert!(!run.timed_out);
    }

    #[test]
    fn a_program_that_exits_zero_is_a_run_with_exit_code_zero() {
        let run = run_stand_in("echo fine", Duration::from_secs(20)).unwrap();
        assert_eq!(run.exit_code, Some(0));
        assert!(!run.timed_out);
    }

    #[test]
    fn a_program_that_does_not_exist_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_captured(
            &dir.path().join("nothing-here"),
            &[],
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(err.starts_with("could not run "), "{err}");
    }

    #[test]
    fn a_program_that_hangs_is_killed_at_the_timeout_and_says_so() {
        let started = Instant::now();
        let run = run_stand_in(SLEEP_A_WHILE, Duration::from_millis(300)).unwrap();
        assert!(run.timed_out, "{run:?}");
        assert_eq!(run.exit_code, None);
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "gave up at the timeout, not when the program ended"
        );
    }
}
