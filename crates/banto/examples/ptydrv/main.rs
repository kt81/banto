//! `ptydrv`: run one program in a pseudo-terminal, under banto's own PTY host,
//! and drive it through files in a control directory.
//!
//! A diagnostic tool, not part of the product. It exists to run a real
//! interactive agent TUI under ConPTY without banto's UI in the way and to
//! measure what banto itself cannot see from outside, such as how long after a
//! write the child answers. See `README.md` for the control directory's
//! contract; it is deliberately not a fixed interface.
//!
//! It opens the child through `banto_io::pty::PortablePtyHost` (the same fork
//! and the same spawn banto's panes use) and models the screen with
//! `banto_core::screen::Screen`, so what it shows is what banto would have
//! shown.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use banto_core::engine::PANE_END_GRACE;
use banto_core::model::PtyExitReason;
use banto_core::screen::Screen;
use banto_io::pty::{
    Hangup, Killer, PortablePtyHost, PtyHost, PtyIo, Resizer, STRIPPED_CHILD_ENV_VARS,
};
use clap::{Args, Parser, Subcommand};

/// How often `in/` is looked at.
const IN_POLL: Duration = Duration::from_millis(200);
/// How often `screen.txt` is rewritten.
const SCREEN_EVERY: Duration = Duration::from_millis(500);
/// How long the loop waits for output before it looks at everything else. Short,
/// so that the time a chunk is logged at is when it arrived and not the next
/// tick.
const OUTPUT_TICK: Duration = Duration::from_millis(10);
/// After the child's exit is known, output still in flight is drained for this
/// long. A dying child's last lines arrive after its exit is reported.
const EXIT_SETTLE: Duration = Duration::from_millis(400);
/// A child stopped before it is this old is logged as `YOUNG`. A Claude Code
/// force-killed while young, twice, turns full-screen rendering off for the
/// whole machine (the README's first line). The driver warns; it does not refuse.
const YOUNG: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(
    name = "ptydrv",
    about = "Run a program in a pseudo-terminal and drive it through files",
    long_about = "Run a program in a pseudo-terminal and drive it through files in --ctl: raw.log, screen.txt, in/, exit.txt and driver.log. See crates/banto/examples/ptydrv/README.md."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the program and drive it until it exits.
    Run(RunArgs),
}

#[derive(Args)]
struct RunArgs {
    /// The child's working directory.
    #[arg(long)]
    cwd: PathBuf,
    /// Control directory. Refused inside the workspace or an agent's home.
    #[arg(long)]
    ctl: PathBuf,
    #[arg(long, default_value_t = 120)]
    cols: u16,
    #[arg(long, default_value_t = 40)]
    rows: u16,
    /// Least time between two writes taken from `in/`, in milliseconds.
    #[arg(long, default_value_t = 300)]
    gap_ms: u64,
    /// Time a hung-up child gets before it is killed, in milliseconds. Defaults
    /// to banto's own pane-end grace.
    #[arg(long)]
    hangup_grace_ms: Option<u64>,
    /// An environment variable added for the child (repeatable).
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_env)]
    env: Vec<(String, String)>,
    /// The program and its arguments, after `--`.
    #[arg(last = true, required = true)]
    program: Vec<String>,
}

fn parse_env(text: &str) -> Result<(String, String), String> {
    match text.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_string(), value.to_string())),
        _ => Err("expected KEY=VALUE".to_string()),
    }
}

#[derive(Clone, Copy)]
struct Timing {
    in_poll: Duration,
    screen_every: Duration,
    output_tick: Duration,
    gap: Duration,
    hangup_grace: Duration,
    exit_settle: Duration,
}

impl Timing {
    fn new(gap: Duration, hangup_grace: Duration) -> Self {
        Self {
            in_poll: IN_POLL,
            screen_every: SCREEN_EVERY,
            output_tick: OUTPUT_TICK,
            gap,
            hangup_grace,
            exit_settle: EXIT_SETTLE,
        }
    }
}

// ---- Pure decisions ---------------------------------------------------------

/// Interpret the escapes in an `in/` file: `\r` `\n` `\t` `\e` `\xNN` `\\`
/// become bytes and any other backslash stays as written. A literal CR or LF in
/// the file is dropped, because an editor or `echo >` ends a line with one and
/// it must not turn into an Enter; so is a leading UTF-8 BOM, which Windows
/// PowerShell 5 writes.
fn unescape(raw: &[u8]) -> Vec<u8> {
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match (raw[i], raw.get(i + 1).copied()) {
            (b'\r' | b'\n', _) => i += 1,
            (b'\\', Some(escape @ (b'r' | b'n' | b't' | b'e' | b'\\'))) => {
                out.push(match escape {
                    b'r' => 0x0d,
                    b'n' => 0x0a,
                    b't' => 0x09,
                    b'e' => 0x1b,
                    _ => b'\\',
                });
                i += 2;
            }
            (b'\\', Some(b'x')) => {
                let byte = raw
                    .get(i + 2..i + 4)
                    .and_then(|hex| std::str::from_utf8(hex).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match byte {
                    Some(byte) => {
                        out.push(byte);
                        i += 4;
                    }
                    None => {
                        out.extend_from_slice(b"\\x");
                        i += 2;
                    }
                }
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    out
}

/// What the loop does about `in/` next.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Nothing,
    /// The first name in order is `hangup`.
    Hangup,
    /// Write this file, once `wait` has passed since the previous write.
    Write {
        name: String,
        wait: Duration,
    },
}

/// Which file of `in/` is next and when it may be written, from the names there,
/// the time of the previous write and `now`. Names go in ascending order. One
/// still being written is named `*.tmp` or `.*` and is not there yet.
///
/// The gap is what lets a body and its Enter, placed together as two files,
/// arrive as two writes: the child's input handling reads one chunk as a paste
/// and a later one as keys (`docs/notes/embedded-pty-spike.md`, "Chunk
/// boundaries carry meaning"), which is why banto's nudge waits 300 ms.
fn plan_input(names: &[String], last_write: Option<Instant>, now: Instant, gap: Duration) -> Plan {
    let first = names
        .iter()
        .filter(|name| !name.ends_with(".tmp") && !name.starts_with('.'))
        .min();
    match first {
        None => Plan::Nothing,
        Some(name) if name == "hangup" => Plan::Hangup,
        Some(name) => Plan::Write {
            name: name.clone(),
            wait: last_write
                .map(|at| (at + gap).saturating_duration_since(now))
                .unwrap_or_default(),
        },
    }
}

/// `2026-11-23T12:27:27.452Z`, from the system clock. No date crate: this is the
/// only place a calendar is needed.
fn utc_stamp(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        since.subsec_millis()
    )
}

/// Whether `ctl` (already resolved) may hold the child's output: not inside any
/// of `protected`. `raw.log` and `screen.txt` are a real session's output, which
/// must not land in the repository (`AGENTS.md` invariant 2) nor in an agent's
/// own home (invariant 1).
fn ctl_allowed(ctl: &Path, protected: &[PathBuf]) -> bool {
    !protected.iter().any(|path| ctl.starts_with(path))
}

// ---- Edges ------------------------------------------------------------------

/// `path` made absolute with every existing part resolved (links, `..`), the
/// rest, which may not exist yet, appended. A path whose missing part ends in
/// `..` cannot be resolved without making it, and is refused.
fn resolve_for_guard(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path).context("cannot make the path absolute")?;
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            bail!("cannot resolve {}", path.display());
        };
        missing.push(name.to_owned());
        existing = existing
            .parent()
            .with_context(|| format!("cannot resolve {}", path.display()))?;
    }
    let mut resolved = existing.canonicalize().context("cannot resolve the path")?;
    resolved.extend(missing.iter().rev());
    Ok(resolved)
}

/// The directories `--ctl` may not be inside: this workspace and the agents'
/// homes, the same set `inspect_input_capture` protects.
fn protected_paths() -> Result<Vec<PathBuf>> {
    let mut protected = Vec::new();
    if let Some(workspace) = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        && let Ok(workspace) = workspace.canonicalize()
    {
        protected.push(workspace);
    }
    for name in ["CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
        if let Some(value) = std::env::var_os(name) {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                bail!("{name} must be an absolute path");
            }
            if let Ok(path) = path.canonicalize() {
                protected.push(path);
            }
        }
    }
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        for name in [".codex", ".claude"] {
            if let Ok(path) = PathBuf::from(&home).join(name).canonicalize() {
                protected.push(path);
            }
        }
    }
    Ok(protected)
}

fn guarded_ctl(ctl: &Path) -> Result<PathBuf> {
    let resolved = resolve_for_guard(ctl)?;
    if !ctl_allowed(&resolved, &protected_paths()?) {
        bail!("--ctl must be outside the banto workspace and the agents' homes");
    }
    Ok(resolved)
}

/// Everything stripped from the child's inherited environment. The agent
/// variable banto always strips, and every `BANTO_*` name set in this process.
///
/// The `BANTO_*` part is this tool's own need and not banto's behaviour: banto
/// hands a brigade member `BANTO_BRIGADE`, `BANTO_MEMBER` and `BANTO_ROLE`, and
/// where its config and database are, so a child started from inside a Worker's
/// pane would otherwise carry that Worker's identity into whatever it is being
/// measured as.
fn env_to_remove() -> Vec<String> {
    let mut names: Vec<String> = STRIPPED_CHILD_ENV_VARS
        .iter()
        .map(|name| name.to_string())
        .collect();
    names.extend(
        std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| name.to_ascii_uppercase().starts_with("BANTO_")),
    );
    names
}

fn launch(host: &dyn PtyHost, args: &RunArgs) -> Result<PtyIo> {
    let remove = env_to_remove();
    let remove: Vec<&str> = remove.iter().map(String::as_str).collect();
    host.open(
        &args.program,
        Some(&args.cwd),
        &args.env,
        &remove,
        args.rows,
        args.cols,
    )
}

/// `<ctl>/driver.log`: what the driver itself did, each line with the wall clock
/// and the milliseconds since the child was started.
struct Diary {
    file: File,
    started: Instant,
}

impl Diary {
    fn line(&mut self, text: &str) {
        let _ = writeln!(
            self.file,
            "{} +{}ms {text}",
            utc_stamp(SystemTime::now()),
            self.started.elapsed().as_millis()
        );
    }
}

/// Replace `screen.txt` in one step, so a reader never sees half a screen. A
/// reader holding the old file open makes the rename fail, and the plain write is
/// the fallback.
fn write_screen(screen: &Screen, path: &Path) {
    let text = screen.screen().contents();
    let tmp = path.with_extension("txt.tmp");
    if fs::write(&tmp, &text).is_ok() && fs::rename(&tmp, path).is_ok() {
        return;
    }
    let _ = fs::remove_file(&tmp);
    let _ = fs::write(path, text);
}

fn list_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

struct Outcome {
    reason: PtyExitReason,
    killed_by_driver: bool,
}

/// What a child can be told, apart from what it writes: the pieces of a
/// [`PtyIo`] that are used after the output has been taken out of it.
struct Handle {
    input: Option<Box<dyn Write + Send>>,
    resizer: Option<Box<dyn Resizer>>,
    killer: Box<dyn Killer>,
    hangup: Box<dyn Hangup>,
}

impl Handle {
    /// Close the child's console as banto does for a pane that is ended
    /// (`embedded::session::PtyHandle::begin_graceful_close`, copied): hang up
    /// unless it already exited, then drop the input and the PTY master on a
    /// thread of their own, since dropping the master is `ClosePseudoConsole` on
    /// Windows, which can wait for the child. banto-io does not offer this order
    /// as one operation, and it is not moved there for a diagnostic tool.
    fn close(&mut self, exited: bool) {
        if !exited {
            let _ = self.hangup.hangup();
        }
        let input = self.input.take();
        let resizer = self.resizer.take();
        let _ = thread::Builder::new()
            .name("pty-close".to_string())
            .spawn(move || {
                drop(input);
                drop(resizer);
            });
    }
}

/// Drive `io` until the child exits: log its output, keep `screen.txt`, type
/// what appears in `in/`, hang up when asked and kill what does not leave.
fn drive(
    io: PtyIo,
    ctl: &Path,
    size: (u16, u16),
    timing: Timing,
    started: Instant,
    diary: &mut Diary,
) -> Result<Outcome> {
    let PtyIo {
        output,
        input,
        resizer,
        killer,
        hangup,
        exited,
        ..
    } = io;
    let mut handle = Handle {
        input: Some(input),
        resizer: Some(resizer),
        killer,
        hangup,
    };
    let mut raw = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ctl.join("raw.log"))?;
    let in_dir = ctl.join("in");
    let screen_path = ctl.join("screen.txt");
    let mut screen = Screen::new(size.0, size.1);

    let mut offset: u64 = 0;
    let mut output_open = true;
    let mut exit_channel_open = true;
    let mut exit: Option<(PtyExitReason, Instant)> = None;
    let mut hung_up_at: Option<Instant> = None;
    let mut killed = false;
    let mut last_write: Option<Instant> = None;
    let mut next_in = Instant::now();
    let mut next_screen = Instant::now();

    loop {
        // Output first: a chunk is logged when it arrives, with its place in raw.log.
        let mut chunks = Vec::new();
        if output_open {
            match output.recv_timeout(timing.output_tick) {
                Ok(chunk) => chunks.push(chunk),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => output_open = false,
            }
            while let Ok(chunk) = output.try_recv() {
                chunks.push(chunk);
            }
        } else {
            thread::sleep(timing.output_tick);
        }
        for chunk in chunks {
            diary.line(&format!("out offset={offset} len={}", chunk.len()));
            raw.write_all(&chunk)?;
            screen.process(&chunk);
            offset += chunk.len() as u64;
        }

        let now = Instant::now();
        if exit.is_none() && exit_channel_open {
            // An error wait is reported as `Unknown` and is not an exit.
            loop {
                match exited.try_recv() {
                    Ok(PtyExitReason::Unknown) => {}
                    Ok(reason) => {
                        diary.line(&format!("child exited: {reason:?}"));
                        exit = Some((reason, now));
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        exit_channel_open = false;
                        break;
                    }
                }
            }
        }

        if now >= next_screen {
            write_screen(&screen, &screen_path);
            next_screen = now + timing.screen_every;
        }

        if let Some((reason, at)) = exit.clone()
            && now.saturating_duration_since(at) >= timing.exit_settle
        {
            write_screen(&screen, &screen_path);
            return Ok(Outcome {
                reason,
                killed_by_driver: killed,
            });
        }

        if let Some(at) = hung_up_at
            && exit.is_none()
            && !killed
            && now.saturating_duration_since(at) >= timing.hangup_grace
        {
            killed = true;
            diary.line(&format!(
                "child still there {} ms after the hangup: killing{}",
                timing.hangup_grace.as_millis(),
                young_note(started, now)
            ));
            let _ = handle.killer.kill();
        }

        if now >= next_in {
            next_in = now + timing.in_poll;
            match plan_input(&list_names(&in_dir), last_write, now, timing.gap) {
                Plan::Nothing => {}
                Plan::Hangup => {
                    let _ = fs::remove_file(in_dir.join("hangup"));
                    if hung_up_at.is_none() {
                        diary.line(&format!("hangup requested{}", young_note(started, now)));
                        hung_up_at = Some(now);
                        handle.close(exit.is_some());
                    }
                }
                Plan::Write { wait, .. } if !wait.is_zero() => next_in = now + wait,
                Plan::Write { name, .. } => {
                    let path = in_dir.join(&name);
                    // A file that cannot be read yet (it is being replaced) is left for the next look.
                    let Ok(raw_input) = fs::read(&path) else {
                        continue;
                    };
                    let bytes = unescape(&raw_input);
                    let result = match handle.input.as_mut() {
                        Some(input) => input.write_all(&bytes).and_then(|()| input.flush()),
                        None => Err(ErrorKind::BrokenPipe.into()),
                    };
                    last_write = Some(now);
                    let _ = fs::remove_file(&path);
                    diary.line(&match result {
                        Ok(()) => format!("in/{name}: wrote {} bytes", bytes.len()),
                        Err(error) => format!("in/{name}: not written ({error})"),
                    });
                    // The next file, if any, waits out the gap.
                    next_in = now;
                }
            }
        }
    }
}

fn young_note(started: Instant, now: Instant) -> String {
    let age = now.saturating_duration_since(started);
    if age < YOUNG {
        format!(" (child age {} ms: YOUNG, under 10 s)", age.as_millis())
    } else {
        format!(" (child age {} ms)", age.as_millis())
    }
}

fn exit_report(outcome: &Outcome, at: SystemTime) -> String {
    let (code, signal) = match &outcome.reason {
        PtyExitReason::Code(code) => (i64::from(*code), String::new()),
        PtyExitReason::Signal(signal) => (-1, signal.clone()),
        PtyExitReason::Unknown => (-1, String::new()),
    };
    format!(
        "exit_code={code}\nsignal={signal}\nexited_at={}\nexited_at_unix_ms={}\nkilled_by_driver={}\n",
        utc_stamp(at),
        at.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        outcome.killed_by_driver
    )
}

fn run(args: RunArgs) -> Result<ExitCode> {
    let ctl = guarded_ctl(&args.ctl)?;
    fs::create_dir_all(ctl.join("in")).context("cannot create the control directory")?;
    let _ = fs::remove_file(ctl.join("exit.txt"));
    let started = Instant::now();
    let mut diary = Diary {
        file: OpenOptions::new()
            .create(true)
            .append(true)
            .open(ctl.join("driver.log"))?,
        started,
    };
    let timing = Timing::new(
        Duration::from_millis(args.gap_ms),
        args.hangup_grace_ms
            .map_or(PANE_END_GRACE, Duration::from_millis),
    );
    let io = match launch(&PortablePtyHost, &args) {
        Ok(io) => io,
        Err(error) => {
            diary.line(&format!("spawn failed: {error:#}"));
            return Err(error.context("cannot start the program"));
        }
    };
    diary.line(&format!(
        "spawned pid={:?} argv={:?} cwd={} size={}x{}",
        io.pid,
        args.program,
        args.cwd.display(),
        args.cols,
        args.rows
    ));
    let outcome = drive(
        io,
        &ctl,
        (args.rows, args.cols),
        timing,
        started,
        &mut diary,
    )?;
    fs::write(
        ctl.join("exit.txt"),
        exit_report(&outcome, SystemTime::now()),
    )?;
    Ok(match outcome.reason {
        PtyExitReason::Code(code) => ExitCode::from(code.min(255) as u8),
        _ => ExitCode::FAILURE,
    })
}

fn main() -> ExitCode {
    let Command::Run(args) = Cli::parse().command;
    match run(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ptydrv: {error:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_io::pty::mock::MockPtyHost;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn escapes_become_bytes() {
        assert_eq!(unescape(b"hi\\r"), b"hi\r");
        assert_eq!(unescape(b"a\\nb\\tc"), b"a\nb\tc");
        assert_eq!(unescape(b"\\e[A"), b"\x1b[A");
        assert_eq!(unescape(b"\\x1b\\x41\\xFF"), [0x1b, b'A', 0xff]);
        assert_eq!(unescape(b"a\\\\b"), b"a\\b");
    }

    #[test]
    fn what_is_not_an_escape_stays_as_written() {
        assert_eq!(unescape(b"\\q"), b"\\q");
        assert_eq!(unescape(b"end\\"), b"end\\");
        assert_eq!(unescape(b"\\x4"), b"\\x4");
        assert_eq!(unescape(b"\\xZZ"), b"\\xZZ");
        assert_eq!(unescape("日本語".as_bytes()), "日本語".as_bytes());
    }

    #[test]
    fn a_line_ending_or_bom_in_the_file_is_not_content() {
        assert_eq!(unescape(b"text\r\n"), b"text");
        assert_eq!(unescape(b"\xEF\xBB\xBFtext\n"), b"text");
        // The Enter has to be written as an escape.
        assert_eq!(unescape(b"text\\r\n"), b"text\r");
    }

    #[test]
    fn files_go_in_name_order_and_unfinished_ones_are_not_there() {
        let now = Instant::now();
        let gap = Duration::from_millis(300);
        let plan = |list: &[&str]| plan_input(&names(list), None, now, gap);
        assert_eq!(plan(&[]), Plan::Nothing);
        assert_eq!(plan(&["001.tmp", ".002", "003.tmp"]), Plan::Nothing);
        assert_eq!(
            plan(&["002-enter", "001-body", "003.tmp"]),
            Plan::Write {
                name: "001-body".to_string(),
                wait: Duration::ZERO
            }
        );
        // `hangup` is an ordinary name in the order: what sorts before it goes first.
        assert_eq!(plan(&["hangup", "zzz"]), Plan::Hangup);
        assert_eq!(
            plan(&["hangup", "001"]),
            Plan::Write {
                name: "001".to_string(),
                wait: Duration::ZERO
            }
        );
    }

    #[test]
    fn a_second_write_waits_out_the_gap_without_sleeping() {
        let base = Instant::now();
        let gap = Duration::from_millis(300);
        let list = names(&["002-enter"]);
        let at = |ms: u64| plan_input(&list, Some(base), base + Duration::from_millis(ms), gap);
        let write = |ms: u64| Plan::Write {
            name: "002-enter".to_string(),
            wait: Duration::from_millis(ms),
        };
        assert_eq!(at(0), write(300));
        assert_eq!(at(120), write(180));
        assert_eq!(at(299), write(1));
        assert_eq!(at(300), write(0));
        assert_eq!(at(5_000), write(0));
        // A hangup is not held back by the gap.
        assert_eq!(
            plan_input(&names(&["hangup"]), Some(base), base, gap),
            Plan::Hangup
        );
    }

    #[test]
    fn the_stamp_is_utc_and_to_the_millisecond() {
        assert_eq!(utc_stamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let at = UNIX_EPOCH + Duration::from_millis(1_795_436_847_452);
        assert_eq!(utc_stamp(at), "2026-11-23T12:27:27.452Z");
        // A leap day.
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(utc_stamp(leap), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn ctl_is_refused_inside_a_protected_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        fs::create_dir(&home).unwrap();
        fs::create_dir(root.join("elsewhere")).unwrap();
        let protected = vec![home.clone()];

        let resolved = |path: &Path| resolve_for_guard(path).unwrap();
        assert!(!ctl_allowed(&resolved(&home), &protected));
        // Not made yet: still inside.
        assert!(!ctl_allowed(
            &resolved(&home.join("a").join("b")),
            &protected
        ));
        // A way round that resolves to inside.
        assert!(!ctl_allowed(
            &resolved(&root.join("elsewhere").join("..").join("home").join("x")),
            &protected
        ));
        assert!(ctl_allowed(
            &resolved(&root.join("elsewhere").join("ctl")),
            &protected
        ));
        // A sibling that merely starts with the same letters is not inside it.
        assert!(ctl_allowed(&resolved(&root.join("home-two")), &protected));
    }

    #[test]
    fn a_dot_dot_through_a_missing_part_cannot_get_out_of_the_guard() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap().join("home");
        fs::create_dir(&home).unwrap();
        let protected = vec![home.clone()];
        // Windows folds the `..` away before anything is looked at and the path is
        // inside; elsewhere it cannot be resolved without making the directory and
        // is an error. Either way the path does not pass.
        let path = home.join("not-made").join("..").join("x");
        let passes =
            resolve_for_guard(&path).is_ok_and(|resolved| ctl_allowed(&resolved, &protected));
        assert!(!passes);
    }

    #[test]
    fn this_workspace_is_protected() {
        let protected = protected_paths().unwrap();
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let ctl = resolve_for_guard(&workspace.join("ctl")).unwrap();
        assert!(!ctl_allowed(&ctl, &protected));
    }

    #[test]
    fn the_child_loses_banto_variables_and_the_one_banto_always_strips() {
        let removed = env_to_remove();
        for name in STRIPPED_CHILD_ENV_VARS {
            assert!(removed.iter().any(|removed| removed == name));
        }
        let args = RunArgs {
            cwd: PathBuf::from("."),
            ctl: PathBuf::from("."),
            cols: 100,
            rows: 30,
            gap_ms: 300,
            hangup_grace_ms: None,
            env: vec![("K".to_string(), "V".to_string())],
            program: vec!["prog".to_string(), "arg".to_string()],
        };
        let host = MockPtyHost::default();
        launch(&host, &args).unwrap();
        let launches = host.launches.lock().unwrap();
        assert_eq!(launches[0].argv, ["prog", "arg"]);
        assert_eq!(launches[0].env, [("K".to_string(), "V".to_string())]);
        let removes = host.env_removes.lock().unwrap();
        assert!(
            removes[0]
                .iter()
                .any(|name| name == STRIPPED_CHILD_ENV_VARS[0])
        );
    }

    fn fast() -> Timing {
        Timing {
            in_poll: Duration::from_millis(5),
            screen_every: Duration::from_millis(20),
            output_tick: Duration::from_millis(2),
            gap: Duration::from_millis(40),
            hangup_grace: Duration::from_millis(150),
            exit_settle: Duration::from_millis(30),
        }
    }

    fn put(ctl: &Path, name: &str, text: &str) {
        let tmp = ctl.join("in").join(format!("{name}.tmp"));
        fs::write(&tmp, text).unwrap();
        fs::rename(&tmp, ctl.join("in").join(name)).unwrap();
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Start `drive` on a mock child in a thread; `ctl` is a temporary directory.
    fn start(host: &MockPtyHost, ctl: &Path) -> thread::JoinHandle<(Outcome, String)> {
        fs::create_dir_all(ctl.join("in")).unwrap();
        let io = host
            .open(&["child".to_string()], None, &[], &[], 24, 80)
            .unwrap();
        let ctl = ctl.to_path_buf();
        thread::spawn(move || {
            let started = Instant::now();
            let mut diary = Diary {
                file: OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(ctl.join("driver.log"))
                    .unwrap(),
                started,
            };
            let outcome = drive(io, &ctl, (24, 80), fast(), started, &mut diary).unwrap();
            (outcome, fs::read_to_string(ctl.join("driver.log")).unwrap())
        })
    }

    #[test]
    fn output_input_and_exit_go_through_the_files() {
        let temp = tempfile::tempdir().unwrap();
        let ctl = temp.path();
        let host = MockPtyHost {
            script: b"hello\r\nworld".to_vec(),
            ..Default::default()
        };
        let running = start(&host, ctl);

        put(ctl, "001-body", "typed");
        put(ctl, "002-enter", "\\r");
        let captured = host.captured.clone();
        wait_for("both writes", || {
            captured.lock().unwrap().as_slice() == b"typed\r"
        });
        host.fire_exit_with_reason(PtyExitReason::Code(3));
        let (outcome, log) = running.join().unwrap();

        assert_eq!(outcome.reason, PtyExitReason::Code(3));
        assert!(!outcome.killed_by_driver);
        assert_eq!(fs::read(ctl.join("raw.log")).unwrap(), b"hello\r\nworld");
        assert!(
            fs::read_to_string(ctl.join("screen.txt"))
                .unwrap()
                .contains("hello\nworld")
        );
        // A chunk is logged with where it sits in raw.log.
        assert!(log.contains("out offset=0 len=12"), "{log}");
        assert!(log.contains("in/001-body: wrote 5 bytes"), "{log}");
        assert!(log.contains("in/002-enter: wrote 1 bytes"), "{log}");
        assert!(
            fs::read_dir(ctl.join("in")).unwrap().next().is_none(),
            "files are taken"
        );
        let report = exit_report(&outcome, UNIX_EPOCH);
        assert!(report.starts_with("exit_code=3\nsignal=\n"), "{report}");
    }

    #[test]
    fn a_hangup_that_is_not_answered_ends_in_a_kill() {
        let temp = tempfile::tempdir().unwrap();
        let ctl = temp.path();
        let host = MockPtyHost::default();
        let running = start(&host, ctl);

        put(ctl, "hangup", "");
        // Files after the hangup have nowhere to go and are not left behind.
        put(ctl, "zzz-late", "late");
        let (outcome, log) = running.join().unwrap();

        assert!(outcome.killed_by_driver);
        assert_eq!(*host.hangups.lock().unwrap(), 1);
        assert_eq!(*host.kills.lock().unwrap(), 1);
        assert!(host.captured.lock().unwrap().is_empty());
        assert!(log.contains("hangup requested (child age"), "{log}");
        assert!(log.contains("YOUNG"), "{log}");
        assert!(log.contains("in/zzz-late: not written"), "{log}");
    }

    #[test]
    fn a_child_that_leaves_after_the_hangup_is_not_killed() {
        let temp = tempfile::tempdir().unwrap();
        let ctl = temp.path();
        let host = MockPtyHost::default();
        let running = start(&host, ctl);

        put(ctl, "hangup", "");
        wait_for("the hangup", || *host.hangups.lock().unwrap() == 1);
        host.fire_exit();
        let (outcome, _) = running.join().unwrap();

        assert!(!outcome.killed_by_driver);
        assert_eq!(*host.kills.lock().unwrap(), 0);
    }
}
