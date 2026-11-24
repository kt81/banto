//! What `codex queue --thread <id> --message <text>` answered, read into one of
//! the outcomes the engine can act on (`banto_io::codex_queue` runs the process
//! and hands this module what it printed).
//!
//! The command appends a message to a thread's queue instead of typing into the
//! pane. On success it exits 0 and prints one line,
//! `Queued message <message_id> for thread <thread_id>.`; aimed at a thread that
//! has not had its first turn it exits non-zero with an error on standard error
//! (`... no rollout found for thread id ...`). Both were run and read by another
//! member of this brigade against Codex 0.171.0; the tests here use synthetic
//! text of that shape, never output captured from a machine.
//!
//! The output is a human-facing line, not an interface Codex promises, so the
//! parse is by the one phrase that carries the ids and an exit 0 that does not
//! match it is its own outcome rather than a guess in either direction.

use serde::{Deserialize, Serialize};

/// Longest stderr or stdout excerpt kept in a result. The result travels in an
/// event the pane log and a replay record, and an error text is shown to a
/// person: neither wants a whole dump.
const SUMMARY_MAX_CHARS: usize = 300;

/// What a finished `codex queue` process left behind, before it is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRun {
    /// `None` when the process was ended by a signal, or killed by banto at its
    /// timeout.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Banto gave up waiting and killed the process.
    pub timed_out: bool,
}

/// How one `codex queue` attempt ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodexQueueResult {
    /// Exit 0 and the success line, with the ids it named. Codex accepted the
    /// message; that it later ran is a fact of the rollout, not of this result.
    Queued {
        message_id: String,
        thread_id: String,
    },
    /// The process ran and reported a failure. `exit_code` is `None` when the
    /// process was killed (a timeout, or a signal): then **it is not known
    /// whether the message was queued**, and sending it again may deliver it
    /// twice. `summary` is the first lines of its standard error, or why it was
    /// killed.
    Failed {
        exit_code: Option<i32>,
        summary: String,
    },
    /// Exit 0, but the output was not the success line. Codex probably queued
    /// the message (it said nothing wrong) and has changed its wording, so this
    /// is neither a success to build on nor a failure to retry.
    UnexpectedOutput { summary: String },
    /// The process could not be started at all: the binary is missing or not
    /// runnable. Nothing was queued.
    NotLaunched { reason: String },
}

/// The arguments `codex queue` is run with, after the program name.
///
/// The message is one `--message` argument, never split or quoted by banto: the
/// operating system's argument passing carries it, newlines included.
pub fn queue_args(thread_id: &str, message: &str) -> Vec<String> {
    vec![
        "queue".to_string(),
        "--thread".to_string(),
        thread_id.to_string(),
        "--message".to_string(),
        message.to_string(),
    ]
}

/// Reads a finished run into an outcome.
pub fn read_queue_run(run: &QueueRun) -> CodexQueueResult {
    if run.timed_out {
        return CodexQueueResult::Failed {
            exit_code: None,
            summary: "timed out; it is not known whether the message was queued".to_string(),
        };
    }
    match run.exit_code {
        Some(0) => match parse_queued_line(&run.stdout) {
            Some((message_id, thread_id)) => CodexQueueResult::Queued {
                message_id,
                thread_id,
            },
            None => CodexQueueResult::UnexpectedOutput {
                summary: excerpt(&run.stdout),
            },
        },
        code => CodexQueueResult::Failed {
            exit_code: code,
            summary: match excerpt(&run.stderr) {
                text if text.is_empty() => excerpt(&run.stdout),
                text => text,
            },
        },
    }
}

/// `Queued message <message_id> for thread <thread_id>.` found on any line of
/// `stdout`. Both ids must be one word each: anything else is not that line.
fn parse_queued_line(stdout: &str) -> Option<(String, String)> {
    stdout.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("Queued message ")?;
        let (message_id, thread) = rest.split_once(" for thread ")?;
        let thread_id = thread.strip_suffix('.').unwrap_or(thread).trim();
        let message_id = message_id.trim();
        let one_word = |id: &str| !id.is_empty() && !id.contains(char::is_whitespace);
        (one_word(message_id) && one_word(thread_id))
            .then(|| (message_id.to_string(), thread_id.to_string()))
    })
}

/// The first few non-empty lines of `text`, joined on one line and cut at
/// [`SUMMARY_MAX_CHARS`].
fn excerpt(text: &str) -> String {
    let joined = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" / ");
    match joined.char_indices().nth(SUMMARY_MAX_CHARS) {
        Some((cut, _)) => format!("{}…", &joined[..cut]),
        None => joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(exit_code: Option<i32>, stdout: &str, stderr: &str) -> QueueRun {
        QueueRun {
            exit_code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            timed_out: false,
        }
    }

    #[test]
    fn the_arguments_carry_the_message_as_one_argument_untouched() {
        let message = "line one\nline two with \"quotes\" & a space";
        assert_eq!(
            queue_args("thread-1", message),
            ["queue", "--thread", "thread-1", "--message", message]
        );
    }

    #[test]
    fn the_success_line_gives_both_ids() {
        let got = read_queue_run(&run(
            Some(0),
            "Queued message msg-0001 for thread thr-0002.\n",
            "",
        ));
        assert_eq!(
            got,
            CodexQueueResult::Queued {
                message_id: "msg-0001".to_string(),
                thread_id: "thr-0002".to_string(),
            }
        );
    }

    #[test]
    fn the_success_line_is_found_among_other_output_and_crlf() {
        let got = read_queue_run(&run(
            Some(0),
            "warning: something\r\nQueued message m1 for thread t1.\r\n",
            "",
        ));
        assert_eq!(
            got,
            CodexQueueResult::Queued {
                message_id: "m1".to_string(),
                thread_id: "t1".to_string(),
            }
        );
    }

    #[test]
    fn a_failure_keeps_the_exit_code_and_the_stderr_text() {
        let got = read_queue_run(&run(
            Some(1),
            "",
            "Error: failed to queue session message: thread/queue/add failed: no rollout found for thread id t1\n",
        ));
        assert_eq!(
            got,
            CodexQueueResult::Failed {
                exit_code: Some(1),
                summary: "Error: failed to queue session message: thread/queue/add failed: no rollout found for thread id t1".to_string(),
            }
        );
    }

    #[test]
    fn a_failure_with_nothing_on_stderr_falls_back_to_stdout() {
        let got = read_queue_run(&run(Some(2), "it did not work\n", "  \n"));
        assert_eq!(
            got,
            CodexQueueResult::Failed {
                exit_code: Some(2),
                summary: "it did not work".to_string(),
            }
        );
    }

    #[test]
    fn a_non_zero_exit_is_a_failure_even_if_it_printed_the_success_line() {
        let got = read_queue_run(&run(
            Some(1),
            "Queued message m1 for thread t1.\n",
            "boom\n",
        ));
        assert!(matches!(
            got,
            CodexQueueResult::Failed {
                exit_code: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn exit_zero_without_the_success_line_is_its_own_outcome() {
        for stdout in [
            "",
            "Queued.\n",
            "Queued message  for thread t1.\n",
            "Queued message m1 for thread two words.\n",
            "Queued message m1\n",
        ] {
            assert!(
                matches!(
                    read_queue_run(&run(Some(0), stdout, "")),
                    CodexQueueResult::UnexpectedOutput { .. }
                ),
                "{stdout:?}"
            );
        }
    }

    #[test]
    fn a_process_ended_by_a_signal_is_a_failure_with_no_exit_code() {
        let got = read_queue_run(&run(None, "", "terminated\n"));
        assert_eq!(
            got,
            CodexQueueResult::Failed {
                exit_code: None,
                summary: "terminated".to_string(),
            }
        );
    }

    #[test]
    fn a_timeout_is_a_failure_that_says_it_is_not_known_whether_the_message_was_queued() {
        let mut timed_out = run(None, "", "");
        timed_out.timed_out = true;
        match read_queue_run(&timed_out) {
            CodexQueueResult::Failed {
                exit_code: None,
                summary,
            } => assert!(summary.contains("not known"), "{summary}"),
            other => panic!("expected an unknown-outcome failure, got {other:?}"),
        }
    }

    #[test]
    fn a_long_error_is_cut_and_a_multibyte_cut_does_not_panic() {
        let long = "エ".repeat(SUMMARY_MAX_CHARS * 2);
        match read_queue_run(&run(Some(1), "", &long)) {
            CodexQueueResult::Failed { summary, .. } => {
                assert_eq!(summary.chars().count(), SUMMARY_MAX_CHARS + 1);
                assert!(summary.ends_with('…'));
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn only_the_first_three_lines_of_stderr_are_kept() {
        let got = read_queue_run(&run(Some(1), "", "a\nb\n\nc\nd\n"));
        assert_eq!(
            got,
            CodexQueueResult::Failed {
                exit_code: Some(1),
                summary: "a / b / c".to_string(),
            }
        );
    }
}
