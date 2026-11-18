//! The file the pane lifecycle log is written to: `panes.log` in banto's logs
//! directory, written by the UI process alone.
//!
//! What a line says and when is decided in `banto_core::pane_log`; this module
//! only admits lines (the rate limit), stamps them, and writes them. Each line
//! is one unbuffered `write_all` on a handle kept open for the process, so a
//! line written before a crash is in the file. Nothing here reports a failure:
//! the log exists to explain the UI, and a full disk or a file held elsewhere
//! must not take the UI down.
//!
//! Rotation renames the current file to `panes.log.1` once the next line would
//! take it past [`MAX_BYTES`]. Nothing is deleted first: the rename replaces the
//! older generation itself, on Windows as on Unix. A rename can fail while
//! another process holds one of the two files without delete sharing (a reader
//! that opens `panes.log` with read and write sharing only does this, as the
//! Windows test below does). A failed rename leaves both generations as they
//! were, and the writer keeps appending to the current file and tries again at
//! the next line. Every file begins with the start line, written again after
//! each rotation, so each file can be read on its own.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use banto_core::engine::GoinkyoLook;
use banto_core::model::BrigadeId;
use banto_core::pane_log::{
    Admission, ChangedOnly, Line, RateLimit, goinkyo_line, suppressed_line,
};

/// The file a process appends to.
pub const FILE_NAME: &str = "panes.log";

/// The one generation kept after rotation.
pub const ROTATED_NAME: &str = "panes.log.1";

/// The size at which the file is rotated. Together with the rotated generation
/// the log holds at most twice this, which at a line of a couple of hundred bytes
/// is some tens of thousands of lines.
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub struct PaneLog {
    dir: PathBuf,
    /// The handle to the current file. `None` after a rotation, or after an open
    /// failed, until the next line opens it again.
    file: Option<File>,
    /// Bytes in the current file, as far as this writer knows them.
    written: u64,
    limit: u64,
    started: Instant,
    rate: RateLimit,
    goinkyo: ChangedOnly<BrigadeId>,
    /// The start line every file begins with.
    start: Option<Line>,
    /// Whether the file in use still has to begin with [`Self::start`].
    start_pending: bool,
}

impl PaneLog {
    /// Opens the log in `logs_dir`, creating the directory if needed. `None` when
    /// the file cannot be opened: the UI then runs without a log, as it would
    /// without this module.
    pub fn open(logs_dir: &Path) -> Option<Self> {
        Self::with_limit(logs_dir, MAX_BYTES)
    }

    /// [`Self::open`] with the rotation size chosen by the caller, for tests.
    pub fn with_limit(logs_dir: &Path, limit: u64) -> Option<Self> {
        fs::create_dir_all(logs_dir).ok()?;
        let file = open_append(&logs_dir.join(FILE_NAME)).ok()?;
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Some(Self {
            dir: logs_dir.to_path_buf(),
            file: Some(file),
            written,
            limit,
            started: Instant::now(),
            rate: RateLimit::default(),
            goinkyo: ChangedOnly::default(),
            start: None,
            start_pending: false,
        })
    }

    /// Writes the start line now, and again at the top of each file a rotation
    /// starts. Not subject to the rate limit: every later line is read against it.
    pub fn start(&mut self, line: &Line) {
        self.start = Some(line.clone());
        self.start_pending = true;
        let since = self.since_start_ms();
        self.ensure_open(since);
    }

    /// Writes a line, or counts it as suppressed when the rate limit is spent.
    pub fn emit(&mut self, line: &Line) {
        let since = self.since_start_ms();
        self.emit_at(line, since);
    }

    pub fn emit_all(&mut self, lines: &[Line]) {
        for line in lines {
            self.emit(line);
        }
    }

    /// Writes the Goinkyo state of a brigade when it differs from the last one
    /// written for that brigade. The decision the same tick makes comes from the
    /// same reading, via [`banto_core::engine::decide_goinkyo`]; this only records it.
    pub fn goinkyo(&mut self, look: &GoinkyoLook) {
        let since = self.since_start_ms();
        self.goinkyo_at(look, since);
    }

    fn goinkyo_at(&mut self, look: &GoinkyoLook, since: u128) {
        let line = goinkyo_line(look);
        let body = line.body();
        if !self.goinkyo.changed(&look.brigade_id, &body) {
            return;
        }
        // Remembered only once it is written, so a line the rate limit drops is
        // still a change on the next tick, and is written then.
        if self.emit_at(&line, since) {
            self.goinkyo.remember(look.brigade_id, &body);
        }
    }

    /// Admits and writes `line` at `since` milliseconds from the start. Returns
    /// whether it was admitted.
    fn emit_at(&mut self, line: &Line, since: u128) -> bool {
        match self.rate.admit(since) {
            Admission::Drop => false,
            Admission::Write { suppressed } => {
                if suppressed > 0 {
                    self.write_line(&suppressed_line(suppressed), since);
                }
                self.write_line(line, since);
                true
            }
        }
    }

    fn since_start_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    fn write_line(&mut self, line: &Line, since: u128) {
        self.ensure_open(since);
        let text = line.render(wall_ms(), since);
        if self.written > 0 && self.written + text.len() as u64 + 1 > self.limit {
            self.rotate();
            self.ensure_open(since);
        }
        self.write_text(&text);
    }

    /// Opens the current file if no handle is held, and writes the start line
    /// first when the file in use has yet to get it.
    fn ensure_open(&mut self, since: u128) {
        if self.file.is_none() {
            self.file = open_append(&self.dir.join(FILE_NAME)).ok();
            self.written = self
                .file
                .as_ref()
                .and_then(|file| file.metadata().ok())
                .map_or(0, |meta| meta.len());
        }
        if !self.start_pending {
            return;
        }
        let Some(start) = self.start.clone() else {
            self.start_pending = false;
            return;
        };
        if self.write_text(&start.render(wall_ms(), since)) {
            self.start_pending = false;
        }
    }

    /// Moves the current file to the rotated name, if the rename can be made. A
    /// rename that fails changes nothing; the next line tries again.
    fn rotate(&mut self) {
        let renamed = fs::rename(self.dir.join(FILE_NAME), self.dir.join(ROTATED_NAME)).is_ok();
        if renamed {
            // The handle belongs to the file that was just renamed. The next open
            // is of the new current file.
            self.file = None;
            self.written = 0;
            self.start_pending = self.start.is_some();
        }
    }

    /// Appends `text` and a newline to the current file. `false` when there is no
    /// file or the write failed.
    fn write_text(&mut self, text: &str) -> bool {
        let Some(file) = self.file.as_mut() else {
            return false;
        };
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(b'\n');
        if file.write_all(&bytes).is_err() {
            return false;
        }
        self.written += bytes.len() as u64;
        true
    }
}

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn wall_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::engine::{GoinkyoGuardFacts, GoinkyoRow, SessionKey};
    use banto_core::pane_log::{RATE_LIMIT_PER_MINUTE, WINDOW_MS, start_line};
    use tempfile::TempDir;

    fn sample(n: u32) -> Line {
        Line::new("PtyOutputLost").num("n", n)
    }

    fn read(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name)).unwrap_or_default()
    }

    fn look(state: &str) -> GoinkyoLook {
        GoinkyoLook {
            brigade_id: 3,
            roster_read: true,
            row: GoinkyoRow::NoSession,
            director_hosted: true,
            guard: GoinkyoGuardFacts {
                guard: Some(SessionKey::from_id("new-worker::3::goinkyo")),
                pane: state == "pane",
                opening: false,
            },
            cwd: PathBuf::from("."),
            roster: Vec::new(),
        }
    }

    const START_BODY: &str = "start version=0.1.0 commit=unknown schema=23 pid=1";

    #[test]
    fn writes_each_line_as_it_is_emitted_in_order() {
        let dir = TempDir::new().unwrap();
        let mut log = PaneLog::open(dir.path()).expect("opens in a temp dir");
        log.start(&start_line("0.1.0", "unknown", 23, 1));
        log.emit(&sample(1));
        log.emit(&sample(2));
        let text = read(dir.path(), FILE_NAME);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with(START_BODY));
        assert!(lines[1].ends_with("PtyOutputLost n=1"));
        assert!(lines[2].ends_with("PtyOutputLost n=2"));
    }

    #[test]
    fn rotates_to_the_previous_generation_at_the_limit() {
        let dir = TempDir::new().unwrap();
        // 30 lines of about 45 bytes against a 1000-byte limit: one rotation,
        // and both generations together keep every line.
        let mut log = PaneLog::with_limit(dir.path(), 1000).expect("opens");
        for n in 0..30 {
            log.emit(&sample(n));
        }
        let current = read(dir.path(), FILE_NAME);
        let previous = read(dir.path(), ROTATED_NAME);
        assert!(!previous.is_empty(), "a rotated generation exists");
        assert!(
            current.len() as u64 <= 1000,
            "the current file stays under the limit"
        );
        assert_eq!(current.lines().count() + previous.lines().count(), 30);
    }

    #[test]
    fn each_file_begins_with_the_start_line() {
        let dir = TempDir::new().unwrap();
        let mut log = PaneLog::with_limit(dir.path(), 400).expect("opens");
        log.start(&start_line("0.1.0", "unknown", 23, 1));
        for n in 0..20 {
            log.emit(&sample(n));
        }
        let current = read(dir.path(), FILE_NAME);
        let previous = read(dir.path(), ROTATED_NAME);
        assert!(!previous.is_empty(), "a rotation happened");
        for text in [&current, &previous] {
            let first = text.lines().next().expect("a first line");
            assert!(first.ends_with(START_BODY), "{first}");
        }
    }

    #[test]
    fn rotation_replaces_an_older_generation() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(ROTATED_NAME), "older generation\n").unwrap();
        let mut log = PaneLog::with_limit(dir.path(), 200).expect("opens");
        for n in 0..20 {
            log.emit(&sample(n));
        }
        assert!(!read(dir.path(), ROTATED_NAME).contains("older generation"));
    }

    #[test]
    fn goinkyo_state_is_written_only_when_it_changes() {
        let dir = TempDir::new().unwrap();
        let mut log = PaneLog::open(dir.path()).expect("opens");
        log.goinkyo(&look("no_pane"));
        log.goinkyo(&look("no_pane"));
        assert_eq!(read(dir.path(), FILE_NAME).lines().count(), 1);
        log.goinkyo(&look("pane"));
        assert_eq!(read(dir.path(), FILE_NAME).lines().count(), 2);
    }

    #[test]
    fn a_goinkyo_change_the_rate_limit_dropped_is_written_when_admitted() {
        let dir = TempDir::new().unwrap();
        let mut log = PaneLog::open(dir.path()).expect("opens");
        for n in 0..RATE_LIMIT_PER_MINUTE {
            log.emit_at(&sample(n), 0);
        }
        log.goinkyo_at(&look("pane"), 0);
        assert_eq!(
            read(dir.path(), FILE_NAME).lines().count(),
            RATE_LIMIT_PER_MINUTE as usize,
            "the state line was dropped with the rest of the storm"
        );
        // The next tick, in a later window, sees the same state and writes it.
        log.goinkyo_at(&look("pane"), WINDOW_MS);
        let text = read(dir.path(), FILE_NAME);
        assert!(text.contains("suppressed count=1"));
        assert!(
            text.lines().last().unwrap().ends_with(
                "goinkyo brigade=3 row=no_session director=hosted roster=read guard=placeholder pane=yes opening=no"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_storm_past_the_rate_limit_writes_one_suppressed_count() {
        let dir = TempDir::new().unwrap();
        let mut log = PaneLog::open(dir.path()).expect("opens");
        for n in 0..(RATE_LIMIT_PER_MINUTE + 50) {
            log.emit_at(&sample(n), 0);
        }
        assert_eq!(
            read(dir.path(), FILE_NAME).lines().count(),
            RATE_LIMIT_PER_MINUTE as usize
        );
        // Once the window rolls, the count is written, then the next admitted line.
        log.emit_at(&sample(9_999), WINDOW_MS);
        let text = read(dir.path(), FILE_NAME);
        assert_eq!(text.matches("suppressed count=50").count(), 1);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[RATE_LIMIT_PER_MINUTE as usize].ends_with("suppressed count=50"));
        assert!(lines[RATE_LIMIT_PER_MINUTE as usize + 1].ends_with("PtyOutputLost n=9999"));
    }

    #[test]
    fn a_log_under_a_file_cannot_be_opened_and_says_nothing() {
        let dir = TempDir::new().unwrap();
        let blocker = dir.path().join("not-a-directory");
        fs::write(&blocker, "x").unwrap();
        assert!(PaneLog::open(&blocker.join("logs")).is_none());
    }

    #[test]
    fn emit_after_the_file_is_removed_stays_silent() {
        let dir = TempDir::new().unwrap();
        let mut log = PaneLog::open(dir.path()).expect("opens");
        // On Windows the open handle leaves the file pending deletion, which an
        // open of it then refuses; on Unix the name is gone. Either way the next
        // emit must not panic, and must not recreate the file with its line in it.
        let _ = fs::remove_file(dir.path().join(FILE_NAME));
        log.emit(&sample(1));
        assert!(read(dir.path(), FILE_NAME).is_empty());
    }

    /// A rotation that the other process's handle makes fail must lose no line,
    /// keep the older generation, and succeed once the handle is gone.
    #[cfg(windows)]
    #[test]
    fn rotation_with_a_reader_holding_the_file_loses_no_line_and_keeps_the_older_generation() {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE: no FILE_SHARE_DELETE, so a rename
        // of the file under this handle is refused.
        const SHARE_READ_WRITE: u32 = 0x1 | 0x2;

        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(ROTATED_NAME), "older generation\n").unwrap();
        let mut log = PaneLog::with_limit(dir.path(), 400).expect("opens");
        log.start(&start_line("0.1.0", "unknown", 23, 1));
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(SHARE_READ_WRITE)
            .open(dir.path().join(FILE_NAME))
            .expect("the reader opens the current file");

        for n in 0..30 {
            log.emit(&sample(n));
        }
        assert_eq!(
            read(dir.path(), ROTATED_NAME),
            "older generation\n",
            "a refused rename leaves the older generation alone"
        );

        drop(reader);
        log.emit(&sample(30));

        let previous = read(dir.path(), ROTATED_NAME);
        let current = read(dir.path(), FILE_NAME);
        assert!(
            !previous.contains("older generation"),
            "the rotation went through"
        );
        for n in 0..=30 {
            let found = previous
                .lines()
                .chain(current.lines())
                .filter(|line| line.ends_with(&format!(" n={n}")))
                .count();
            assert_eq!(found, 1, "line {n} is kept exactly once");
        }
    }
}
