use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use banto_core::app::App;
use banto_core::config::BrigadeConfig;
use banto_core::engine::{self, EmporiumState, PrefixKey};
use banto_core::program_state::ReportedState;
use banto_core::replay::{TimedEvent, parse_stream};
use clap::{CommandFactory, Parser, error::ErrorKind};

const MAX_COLUMNS: u16 = 160;

#[derive(Parser)]
#[command(
    name = "inspect_input_capture",
    version,
    about = "Inspect anonymized screen metadata from an isolated capture",
    long_about = "Inspect anonymized screen metadata from an isolated capture. Replay starts with an empty session list, the default prefix and default brigade configuration. Generated commands are not executed; the recording must contain their feedback events. Cell classes are diagnostic metadata and cannot determine whether input is empty."
)]
struct Args {
    /// Event stream inside the system temporary directory.
    #[arg(long)]
    input: PathBuf,
    /// One-based event indices, sampled immediately after each event.
    #[arg(long, required = true, value_delimiter = ',')]
    event: Vec<usize>,
    /// Number of bottom rows to inspect (1 through 12).
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=12))]
    rows: u16,
    /// First column of the inspected window, zero-based.
    #[arg(long, default_value_t = 0)]
    column_start: u16,
    /// Compare the candidate prompt row with two fixed public placeholders.
    #[arg(long)]
    public_placeholder_check: bool,
}

// Codex rust-v0.169.0, tui/src/chatwidget.rs:2058–2059.
const NORMAL_PLACEHOLDER: &str = "Ask Codex to do anything";
const SIDE_PLACEHOLDER: &str = "Ask a follow-up question";

fn public_placeholder_match(screen: &banto_core::screen::Screen) -> Option<(bool, bool)> {
    let (height, width) = screen.size();
    let row = height.checked_sub(4)?;
    if width < 26 {
        return None;
    }
    let view = screen.screen();
    let mut normal = true;
    let mut side = true;
    for (i, (normal_byte, side_byte)) in NORMAL_PLACEHOLDER
        .bytes()
        .zip(SIDE_PLACEHOLDER.bytes())
        .enumerate()
    {
        let cell = view.cell(row, 2 + i as u16)?;
        let value = cell.contents();
        if cell.is_wide() || cell.is_wide_continuation() || value.chars().count() > 1 {
            return None;
        }
        let bytes = value.as_bytes();
        normal &= bytes == [normal_byte];
        side &= bytes == [side_byte];
    }
    Some((normal, side))
}

fn glyph_class(s: &str) -> String {
    if s.is_empty() {
        return "empty".into();
    }
    if s.chars().all(char::is_whitespace) {
        return match s {
            " " => "ascii-space",
            "\u{a0}" => "no-break-space",
            "\u{3000}" => "wide-space",
            _ => "other-whitespace",
        }
        .into();
    }
    if s.chars().any(char::is_alphabetic) {
        return "letter".into();
    }
    if s.chars().any(char::is_numeric) {
        return "digit".into();
    }
    if s.chars().count() == 1 {
        let c = s.chars().next().unwrap();
        if matches!(
            c,
            '|' | '-'
                | '+'
                | '>'
                | '<'
                | '─'
                | '│'
                | '┌'
                | '┐'
                | '└'
                | '┘'
                | '├'
                | '┤'
                | '┬'
                | '┴'
                | '┼'
                | '╭'
                | '╮'
                | '╰'
                | '╯'
                | '›'
                | '!'
                | '↑'
                | '↓'
        ) {
            return format!("U+{:04X}", c as u32);
        }
        return match c as u32 {
            0x2500..=0x257f => "box",
            0x2580..=0x259f => "block",
            0x2190..=0x21ff => "arrow",
            0x25a0..=0x25ff => "geometric",
            _ => "other",
        }
        .into();
    }
    "other".into()
}

fn bottom_rows(height: u16, requested: u16) -> Range<u16> {
    height.saturating_sub(requested)..height
}

fn describe_screen(
    screen: &banto_core::screen::Screen,
    rows: u16,
    column_start: u16,
) -> Result<String> {
    let (height, width) = screen.size();
    if column_start >= width {
        bail!("column start is outside the pane");
    }
    let view = screen.screen();
    let (cursor_row, cursor_col) = view.cursor_position();
    let shown = width.min(column_start.saturating_add(MAX_COLUMNS));
    let mut out = format!(
        "diagnostic classes only; identical classes do not establish identical text or an empty input\nsize={height}x{width} cursor={cursor_row},{cursor_col} cursor_visible={} scrollback={} reported_state={} synchronized_update={} columns={column_start}..{shown} clipped_left={} clipped_right={}\n",
        !view.hide_cursor(),
        screen.scrollback(),
        match screen.reported_state() {
            None => "none",
            Some(ReportedState::Idle) => "idle",
            Some(ReportedState::Working) => "working",
            Some(ReportedState::Blocked(_)) => "blocked",
            Some(ReportedState::Done) => "done",
            Some(ReportedState::Error) => "error",
        },
        screen.in_synchronized_update(),
        column_start > 0,
        shown < width
    );
    for row in bottom_rows(height, rows) {
        let mut previous = String::new();
        let mut start = column_start;
        for col in column_start..=shown {
            let current = if col == shown {
                String::new()
            } else {
                let cell = view.cell(row, col).expect("cell within screen");
                format!(
                    "{} fg={:?} bg={:?} bold={} dim={} italic={} underline={} inverse={} wide={} continuation={}",
                    glyph_class(cell.contents()),
                    cell.fgcolor(),
                    cell.bgcolor(),
                    cell.bold(),
                    cell.dim(),
                    cell.italic(),
                    cell.underline(),
                    cell.inverse(),
                    cell.is_wide(),
                    cell.is_wide_continuation()
                )
            };
            if current != previous {
                if !previous.is_empty() {
                    let _ = writeln!(out, "row={row} cols={start}..{col} {previous}");
                }
                start = col;
                previous = current;
            }
        }
    }
    Ok(out)
}

fn inspect(
    events: &[TimedEvent],
    samples: &[usize],
    rows: u16,
    column_start: u16,
    public_placeholder_check: bool,
) -> Result<String> {
    if samples
        .iter()
        .any(|&index| index == 0 || index > events.len())
    {
        bail!("sample index outside event stream");
    }
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let brigade = BrigadeConfig::default();
    let base = Instant::now();
    let mut out = String::new();
    for (index, timed) in events.iter().enumerate() {
        let now = base
            .checked_add(Duration::from_millis(timed.offset_ms))
            .context("invalid event offset")?;
        let _ = engine::update(&mut state, &mut app, &brigade, timed.event.clone(), now);
        if samples.contains(&(index + 1)) {
            if state.screens.len() != 1 {
                bail!("sample requires exactly one pane");
            }
            let screen = state.screens.values().next().expect("one pane");
            let _ = writeln!(out, "event={} offset_ms={}", index + 1, timed.offset_ms);
            out.push_str(&describe_screen(screen, rows, column_start)?);
            if public_placeholder_check {
                match public_placeholder_match(screen) {
                    Some((normal, side)) => {
                        let _ = writeln!(
                            out,
                            "public_placeholder normal_match={normal} side_match={side}"
                        );
                    }
                    None => out.push_str("public_placeholder unavailable\n"),
                }
            }
        }
    }
    Ok(out)
}

fn allowed_input(input: &Path, temp: &Path, protected: &[PathBuf]) -> bool {
    input.starts_with(temp) && !protected.iter().any(|path| input.starts_with(path))
}

fn run(args: Args) -> Result<()> {
    let temp = std::env::temp_dir()
        .canonicalize()
        .context("cannot resolve temporary directory")?;
    let input = args
        .input
        .canonicalize()
        .context("cannot resolve input path")?;
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .context("cannot determine workspace")?
        .canonicalize()
        .context("cannot resolve workspace")?;
    let mut protected = vec![workspace];
    for name in ["CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
        if let Some(value) = std::env::var_os(name) {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                bail!("agent home must be an absolute path");
            }
            if path.exists() {
                protected.push(path.canonicalize().context("cannot resolve agent home")?);
            }
        }
    }
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        for name in [".codex", ".claude"] {
            let path = PathBuf::from(&home).join(name);
            if path.exists() {
                protected.push(path.canonicalize().context("cannot resolve agent home")?);
            }
        }
    }
    if !allowed_input(&input, &temp, &protected) {
        bail!("input must be in the temporary directory outside the workspace and agent homes");
    }
    let text = std::fs::read_to_string(input).context("cannot read input")?;
    // Parsing errors may contain untrusted content; never echo them.
    let events = parse_stream(&text).map_err(|_| anyhow::anyhow!("invalid event stream"))?;
    print!(
        "{}",
        inspect(
            &events,
            &args.event,
            args.rows,
            args.column_start,
            args.public_placeholder_check
        )?
    );
    Ok(())
}

fn main() {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            let _ = Args::command().print_long_help();
            println!();
            return;
        }
        Err(error) if error.kind() == ErrorKind::DisplayVersion => {
            println!("inspect_input_capture {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Err(error) => {
            eprintln!("{}", safe_argument_error(&error));
            std::process::exit(2);
        }
    };
    if let Err(error) = run(args) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn safe_argument_error(_: &clap::Error) -> &'static str {
    "invalid arguments; use --help"
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::engine::{Event, SessionKey};

    #[test]
    fn classes_never_expose_words_or_digits() {
        assert_eq!(glyph_class("A"), "letter");
        assert_eq!(glyph_class("9"), "digit");
        assert_eq!(glyph_class("界"), "letter");
        assert_eq!(glyph_class("|"), "U+007C");
        assert_eq!(glyph_class("@"), "other");
        assert_eq!(glyph_class(" "), "ascii-space");
        assert_eq!(glyph_class("\u{a0}"), "no-break-space");
        assert_eq!(glyph_class("\u{3000}"), "wide-space");
        assert_eq!(glyph_class("\t"), "other-whitespace");
        assert_eq!(glyph_class("▓"), "block");
        assert_eq!(glyph_class("→"), "arrow");
        assert_eq!(glyph_class("◆"), "geometric");
        assert_eq!(glyph_class("›"), "U+203A");
        assert_eq!(glyph_class("!"), "U+0021");
        assert_eq!(glyph_class("↑"), "U+2191");
        assert_eq!(glyph_class("↓"), "U+2193");
        assert_eq!(glyph_class("e\u{301}"), "letter");
        assert_eq!(glyph_class("\u{1b}"), "other");
    }

    #[test]
    fn cli_errors_never_echo_values() {
        let error = Args::try_parse_from([
            "example",
            "--input",
            "PRIVATE_SENTINEL",
            "--event",
            "PRIVATE_SENTINEL",
        ])
        .err()
        .expect("invalid index");
        assert!(!safe_argument_error(&error).contains("PRIVATE_SENTINEL"));
        assert_eq!(
            Args::try_parse_from(["example", "--help"])
                .err()
                .unwrap()
                .kind(),
            ErrorKind::DisplayHelp
        );
    }

    #[test]
    fn protected_input_paths_are_rejected() {
        let temp = Path::new("/tmp");
        let protected = vec![PathBuf::from("/tmp/repo"), PathBuf::from("/tmp/.codex")];
        assert!(allowed_input(
            Path::new("/tmp/capture/events"),
            temp,
            &protected
        ));
        assert!(!allowed_input(
            Path::new("/tmp/repo/events"),
            temp,
            &protected
        ));
        assert!(!allowed_input(
            Path::new("/tmp/.codex/events"),
            temp,
            &protected
        ));
        assert!(!allowed_input(Path::new("/other/events"), temp, &protected));
    }

    #[test]
    fn wide_screens_and_styled_wide_cells_are_bounded() {
        let mut screen = banto_core::screen::Screen::new(8, 400);
        screen.process("\x1b[8;1H\x1b[31;1m界e\u{301}".as_bytes());
        let output = describe_screen(&screen, 1, 0).unwrap();
        assert!(output.contains("columns=0..160 clipped_left=false clipped_right=true"));
        assert!(output.contains("wide=true"));
        assert!(output.contains("continuation=true"));
        assert!(output.contains("bold=true"));
        assert!(!output.contains("界"));
        assert!(!output.contains("e\u{301}"));
        assert!(!output.contains("cols=160"));
        let middle = describe_screen(&screen, 1, 160).unwrap();
        assert!(middle.contains("columns=160..320 clipped_left=true clipped_right=true"));
        assert!(middle.contains("row=7 cols=160..320 empty"));
        assert!(!middle.contains("row=7 cols=0.."));
        assert!(describe_screen(&screen, 1, 400).is_err());
    }

    #[test]
    fn dim_cursor_visibility_and_exclusive_ranges_are_reported() {
        let mut screen = banto_core::screen::Screen::new(8, 20);
        screen.process(b"\x1b[8;1H\x1b[2mA\x1b[22mB\x1b[?25l");
        let hidden = describe_screen(&screen, 1, 0).unwrap();
        assert!(hidden.contains("cursor_visible=false"));
        assert!(hidden.contains("scrollback=0"));
        assert!(hidden.contains("row=7 cols=0..1 letter"));
        assert!(hidden.contains("row=7 cols=1..2 letter"));
        assert!(hidden.contains("dim=true"));
        assert!(hidden.contains("dim=false"));
        screen.process(b"\x1b[?25h");
        assert!(
            describe_screen(&screen, 1, 0)
                .unwrap()
                .contains("cursor_visible=true")
        );
    }

    #[test]
    fn program_state_and_synchronized_update_are_safe_labels() {
        let mut screen = banto_core::screen::Screen::new(8, 80);
        let initial = describe_screen(&screen, 1, 0).unwrap();
        assert!(initial.contains("reported_state=none synchronized_update=false"));
        screen.process(b"\x1b]7501;state=blocked:kind=permission\x07\x1b[?2026h");
        let active = describe_screen(&screen, 1, 0).unwrap();
        assert!(active.contains("reported_state=blocked synchronized_update=true"));
        screen.process(b"\x1b]7501;state=idle\x07\x1b[?2026l");
        let complete = describe_screen(&screen, 1, 0).unwrap();
        assert!(complete.contains("reported_state=idle synchronized_update=false"));
    }

    #[test]
    fn public_placeholder_comparison_is_fixed_and_bounded() {
        let mut normal = banto_core::screen::Screen::new(8, 80);
        normal.process(format!("\x1b[5;3H{NORMAL_PLACEHOLDER}").as_bytes());
        assert_eq!(public_placeholder_match(&normal), Some((true, false)));
        let mut side = banto_core::screen::Screen::new(8, 80);
        side.process(format!("\x1b[5;3H{SIDE_PLACEHOLDER}").as_bytes());
        assert_eq!(public_placeholder_match(&side), Some((false, true)));
        let mut private = banto_core::screen::Screen::new(8, 80);
        private.process(b"\x1b[5;3HPRIVATE_SENTINEL");
        assert_eq!(public_placeholder_match(&private), Some((false, false)));
        let output = describe_screen(&private, 8, 0).unwrap();
        assert!(!output.contains("PRIVATE_SENTINEL"));
        let narrow = banto_core::screen::Screen::new(8, 20);
        assert_eq!(public_placeholder_match(&narrow), None);
        let mut wide = banto_core::screen::Screen::new(8, 80);
        wide.process("\x1b[5;3H界".as_bytes());
        assert_eq!(public_placeholder_match(&wide), None);
        let mut combined = banto_core::screen::Screen::new(8, 80);
        combined.process("\x1b[5;3He\u{301}".as_bytes());
        assert_eq!(public_placeholder_match(&combined), None);
    }

    #[test]
    fn bounds_and_cursor_are_clamped() {
        assert_eq!(bottom_rows(3, 8), 0..3);
        assert_eq!(bottom_rows(20, 6), 14..20);
        let screen = banto_core::screen::Screen::new(8, 20);
        let output = describe_screen(&screen, 6, 0).unwrap();
        assert!(output.contains("cursor=0,0"));
        assert!(!output.contains("row=1 "));
        assert!(output.contains("row=2 "));
    }

    #[test]
    fn samples_incremental_spawn_and_output_without_text() {
        let key = SessionKey::from_id("synthetic-id");
        let events = vec![
            TimedEvent {
                offset_ms: 0,
                event: Event::Spawned {
                    key: key.clone(),
                    agent: None,
                },
            },
            TimedEvent {
                offset_ms: 1,
                event: Event::PtyOutput {
                    key,
                    chunk: b"\x1b[24;1HSecret42".to_vec(),
                },
            },
        ];
        let output = inspect(&events, &[1, 2], 8, 0, false).unwrap();
        assert!(output.contains("event=1"));
        assert!(output.contains("event=2"));
        assert!(output.contains("letter"));
        assert!(output.contains("digit"));
        assert!(!output.contains("Secret"));
        assert!(!output.contains("synthetic-id"));
    }

    #[test]
    fn samples_reject_multiple_panes_and_invalid_indices() {
        let events = vec![
            TimedEvent {
                offset_ms: 0,
                event: Event::Spawned {
                    key: SessionKey::from_id("first"),
                    agent: None,
                },
            },
            TimedEvent {
                offset_ms: 1,
                event: Event::Spawned {
                    key: SessionKey::from_id("second"),
                    agent: None,
                },
            },
        ];
        assert!(inspect(&events, &[2], 8, 0, false).is_err());
        assert!(inspect(&events, &[0], 8, 0, false).is_err());
        assert!(inspect(&events, &[3], 8, 0, false).is_err());
    }
}
