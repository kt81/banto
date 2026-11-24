//! Running `codex queue` off the event loop, one request at a time.
//!
//! The process takes about 0.2 s and is bounded at five (`banto_io::codex_queue`),
//! and the loop is also what draws the screen, so the request is handed to a
//! thread and its answer comes back as an `Event` in a later iteration: the
//! loop drains [`CodexQueue::answers`] right after it reads its input. The
//! thread only ever turns a request into an event. It touches no state, and
//! what a request that was never answered means is the core's to decide
//! (`docs/DISCIPLINE.md` §6).
//!
//! One thread, not one per request: two wakes for the same member must not be
//! in flight together (a double delivery is exactly what the wake's judgement
//! exists to avoid), and one at a time keeps that true without the core having
//! to know it.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};

use banto_core::codex_queue::{CodexQueueResult, read_queue_run};
use banto_core::engine::{Event, SessionKey};
use banto_io::codex_queue::CodexQueueSender;

/// One `codex queue` to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRequest {
    pub key: SessionKey,
    pub session_id: String,
    pub binary: String,
    pub message: String,
}

/// Where the loop sends a `Cmd::QueueCodexMessage` and collects what came of it.
pub trait CodexQueue {
    /// Hand `request` over to run. Returns the events that are already known:
    /// none in the ordinary case, and the answer itself when it cannot be run at
    /// all, so a request is never silently lost.
    fn submit(&self, request: QueueRequest) -> Vec<Event>;
    /// The answers that have arrived since the last call, in the order the
    /// requests were submitted. Never waits.
    fn answers(&self) -> Vec<Event>;
}

/// [`CodexQueue`] over one thread that runs the requests in order.
pub struct CodexQueueWorker {
    jobs: Sender<QueueRequest>,
    answers: Receiver<Event>,
    // Held so the thread is not detached by accident; never joined: a request
    // is bounded by its own timeout, and the loop must not wait for it on the
    // way out.
    _thread: JoinHandle<()>,
}

impl CodexQueueWorker {
    pub fn spawn(sender: Box<dyn CodexQueueSender + Send>) -> Self {
        let (jobs, requests) = channel::<QueueRequest>();
        let (replies, answers) = channel::<Event>();
        let thread = thread::Builder::new()
            .name("codex-queue".to_string())
            .spawn(move || {
                // Ends when the worker is dropped (the request channel closes) or
                // when nobody is left to read the answers.
                for request in requests {
                    let event = answer_to(&*sender, request);
                    if replies.send(event).is_err() {
                        break;
                    }
                }
            })
            .expect("a thread can be spawned");
        Self {
            jobs,
            answers,
            _thread: thread,
        }
    }
}

/// Run the request and read what the process printed into an outcome. A process
/// that could not be started is an outcome of its own, not an error to swallow.
fn answer_to(sender: &dyn CodexQueueSender, request: QueueRequest) -> Event {
    let result = match sender.queue(&request.binary, &request.session_id, &request.message) {
        Ok(run) => read_queue_run(&run),
        Err(reason) => CodexQueueResult::NotLaunched { reason },
    };
    Event::CodexMessageQueued {
        key: request.key,
        session_id: request.session_id,
        result,
    }
}

impl CodexQueue for CodexQueueWorker {
    fn submit(&self, request: QueueRequest) -> Vec<Event> {
        match self.jobs.send(request) {
            Ok(()) => Vec::new(),
            Err(failed) => {
                let request = failed.0;
                vec![Event::CodexMessageQueued {
                    key: request.key,
                    session_id: request.session_id,
                    result: CodexQueueResult::NotLaunched {
                        reason: "the thread that runs `codex queue` has ended".to_string(),
                    },
                }]
            }
        }
    }

    fn answers(&self) -> Vec<Event> {
        self.answers.try_iter().collect()
    }
}

/// A sender that answers its first two requests at once and holds the third
/// until told to go: the only way for a test to have exactly two answers
/// waiting, without taking them to see (`answers` drains). The thread runs
/// requests in order, so when the third has started the first two are answered.
#[cfg(test)]
pub(super) struct PausesOnThird {
    calls: std::sync::atomic::AtomicUsize,
    reached: std::sync::Mutex<Sender<()>>,
    release: std::sync::Mutex<Receiver<()>>,
}

#[cfg(test)]
impl PausesOnThird {
    /// The sender, the channel that says the third request has started, and the
    /// one that lets it finish.
    pub(super) fn new() -> (Self, Receiver<()>, Sender<()>) {
        let (reached, started) = channel();
        let (go, release) = channel();
        let sender = Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            reached: std::sync::Mutex::new(reached),
            release: std::sync::Mutex::new(release),
        };
        (sender, started, go)
    }
}

#[cfg(test)]
impl CodexQueueSender for PausesOnThird {
    fn queue(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<banto_core::codex_queue::QueueRun, String> {
        let nth = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if nth == 2 {
            let _ = self.reached.lock().unwrap().send(());
            // A test that failed before releasing drops its end: this returns.
            let _ = self.release.lock().unwrap().recv();
        }
        Ok(banto_core::codex_queue::QueueRun {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: format!("refused {nth}\n"),
            timed_out: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use banto_core::codex_queue::QueueRun;

    use super::*;

    /// Answers from a script and records what it was asked, in order, with how
    /// many were running at once at the busiest.
    #[derive(Clone)]
    struct Scripted {
        answers: Arc<Mutex<Vec<Result<QueueRun, String>>>>,
        asked: Arc<Mutex<Vec<(String, String, String)>>>,
        running: Arc<Mutex<(usize, usize)>>,
    }

    impl Scripted {
        fn new(answers: Vec<Result<QueueRun, String>>) -> Self {
            Self {
                answers: Arc::new(Mutex::new(answers)),
                asked: Arc::default(),
                running: Arc::default(),
            }
        }
    }

    impl CodexQueueSender for Scripted {
        fn queue(&self, binary: &str, thread: &str, message: &str) -> Result<QueueRun, String> {
            {
                let mut running = self.running.lock().unwrap();
                running.0 += 1;
                running.1 = running.1.max(running.0);
            }
            self.asked.lock().unwrap().push((
                binary.to_string(),
                thread.to_string(),
                message.to_string(),
            ));
            std::thread::sleep(Duration::from_millis(30));
            self.running.lock().unwrap().0 -= 1;
            self.answers.lock().unwrap().remove(0)
        }
    }

    fn run_of(exit_code: Option<i32>, stdout: &str, stderr: &str) -> QueueRun {
        QueueRun {
            exit_code,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            timed_out: false,
        }
    }

    fn request(thread: &str, message: &str) -> QueueRequest {
        QueueRequest {
            key: SessionKey::from_id(thread),
            session_id: thread.to_string(),
            binary: "codex".to_string(),
            message: message.to_string(),
        }
    }

    /// Wait (bounded) until `count` answers have arrived.
    fn collect(queue: &CodexQueueWorker, count: usize) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while got.len() < count && Instant::now() < deadline {
            got.extend(queue.answers());
            std::thread::sleep(Duration::from_millis(5));
        }
        got
    }

    #[test]
    fn submitting_never_waits_for_the_process() {
        let scripted = Scripted::new(vec![Ok(run_of(
            Some(0),
            "Queued message m-1 for thread t-1.\n",
            "",
        ))]);
        let queue = CodexQueueWorker::spawn(Box::new(scripted.clone()));
        let started = Instant::now();
        let at_once = queue.submit(request("t-1", "wake"));
        assert!(at_once.is_empty(), "{at_once:?}");
        assert!(
            started.elapsed() < Duration::from_millis(25),
            "the request ran on the caller's thread: {:?}",
            started.elapsed()
        );
        assert!(queue.answers().is_empty(), "the process had not finished");
        assert_eq!(collect(&queue, 1).len(), 1);
    }

    #[test]
    fn the_message_goes_whole_and_the_answer_names_what_codex_named() {
        let scripted = Scripted::new(vec![Ok(run_of(
            Some(0),
            "Queued message m-1 for thread t-1.\n",
            "",
        ))]);
        let queue = CodexQueueWorker::spawn(Box::new(scripted.clone()));
        queue.submit(request("t-1", "first line\nsecond line"));
        assert_eq!(
            collect(&queue, 1),
            vec![Event::CodexMessageQueued {
                key: SessionKey::from_id("t-1"),
                session_id: "t-1".to_string(),
                result: CodexQueueResult::Queued {
                    message_id: "m-1".to_string(),
                    thread_id: "t-1".to_string(),
                },
            }]
        );
        assert_eq!(
            *scripted.asked.lock().unwrap(),
            [(
                "codex".to_string(),
                "t-1".to_string(),
                "first line\nsecond line".to_string()
            )]
        );
    }

    #[test]
    fn requests_run_one_at_a_time_and_answer_in_order() {
        let ok = |id: &str| {
            Ok(run_of(
                Some(0),
                &format!("Queued message {id} for thread t.\n"),
                "",
            ))
        };
        let scripted = Scripted::new(vec![ok("m-1"), ok("m-2"), ok("m-3")]);
        let queue = CodexQueueWorker::spawn(Box::new(scripted.clone()));
        for thread in ["a", "b", "c"] {
            queue.submit(request(thread, "wake"));
        }
        let got = collect(&queue, 3);
        let ids: Vec<String> = got
            .iter()
            .map(|event| match event {
                Event::CodexMessageQueued { session_id, .. } => session_id.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(scripted.running.lock().unwrap().1, 1, "never two at once");
    }

    #[test]
    fn answers_hands_over_every_answer_that_has_piled_up_in_order_and_at_once() {
        let (sender, third_started, release) = PausesOnThird::new();
        let queue = CodexQueueWorker::spawn(Box::new(sender));
        for thread in ["a", "b", "c"] {
            queue.submit(request(thread, "wake"));
        }
        third_started
            .recv_timeout(Duration::from_secs(10))
            .expect("the third request started");

        let piled: Vec<String> = queue
            .answers()
            .into_iter()
            .map(|event| match event {
                Event::CodexMessageQueued { session_id, .. } => session_id,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(piled, ["a", "b"], "both, in submission order, in one call");
        assert!(queue.answers().is_empty(), "and then there is nothing");

        release.send(()).unwrap();
        assert_eq!(collect(&queue, 1).len(), 1);
    }

    #[test]
    fn a_refusal_a_missing_codex_and_an_odd_answer_are_each_an_outcome() {
        let scripted = Scripted::new(vec![
            Ok(run_of(Some(1), "", "Error: no rollout found\n")),
            Err("could not run codex: not found".to_string()),
            Ok(run_of(Some(0), "ok, noted\n", "")),
        ]);
        let queue = CodexQueueWorker::spawn(Box::new(scripted));
        for thread in ["a", "b", "c"] {
            queue.submit(request(thread, "wake"));
        }
        let results: Vec<CodexQueueResult> = collect(&queue, 3)
            .into_iter()
            .map(|event| match event {
                Event::CodexMessageQueued { result, .. } => result,
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(
            matches!(
                &results[0],
                CodexQueueResult::Failed { exit_code: Some(1), summary } if summary.contains("no rollout found")
            ),
            "{results:?}"
        );
        assert_eq!(
            results[1],
            CodexQueueResult::NotLaunched {
                reason: "could not run codex: not found".to_string()
            }
        );
        assert!(
            matches!(&results[2], CodexQueueResult::UnexpectedOutput { .. }),
            "{results:?}"
        );
    }

    #[test]
    fn a_request_that_cannot_be_handed_over_is_answered_not_lost() {
        let queue = CodexQueueWorker::spawn(Box::new(Scripted::new(Vec::new())));
        // A thread that has ended: replace the channel's receiving side.
        let (jobs, requests) = channel::<QueueRequest>();
        drop(requests);
        let queue = CodexQueueWorker { jobs, ..queue };
        let events = queue.submit(request("t-1", "wake"));
        assert!(
            matches!(
                events.as_slice(),
                [Event::CodexMessageQueued {
                    result: CodexQueueResult::NotLaunched { .. },
                    ..
                }]
            ),
            "{events:?}"
        );
    }
}
