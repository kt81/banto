//! Embedded terminal host: run a child (e.g. `claude --resume <id>`) inside
//! banto's own ratatui TUI — spawn it in a PTY, parse its output with `vt100`,
//! render the grid, and forward input. This is the productionised form of the
//! `docs/notes/embedded-pty-spike.md` case-A spike (banto as a minimal
//! multiplexer), Slice 1: a single embedded pane.
//!
//! Windows/ConPTY note (from the spike): banto must NOT answer the child's
//! DSR/DA queries — ConPTY is the child's terminal and passes host input
//! straight to the child's stdin, so answering leaks the reply as typed input.
//! A raw Unix PTY would need answering; that path is deferred.

mod convert;
mod emporium;
mod session;

pub use banto_io::pty::PortablePtyHost;
pub use banto_tui::render::screen_to_text;
pub use emporium::run as run_emporium;
pub use session::EmbeddedSession;

use std::io::Stdout;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;
use ratatui::widgets::{Block, Paragraph};

/// Host `argv` in a full-screen embedded pane until the user presses F12.
/// Reached via the hidden `banto _embed` subcommand; the list/brigade wiring
/// (Slice 1b / Slice 2) will call the same [`EmbeddedSession`] primitives.
pub fn run_embedded(argv: &[String], cwd: Option<&Path>) -> Result<()> {
    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    let result = embedded_loop(&mut terminal, argv, cwd);

    disable_raw_mode()?;
    let _ = std::io::stdout().execute(LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

fn embedded_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    argv: &[String],
    cwd: Option<&Path>,
) -> Result<()> {
    let (w, h) = crossterm::terminal::size()?;
    let host = PortablePtyHost;
    let mut session = EmbeddedSession::open(
        &host,
        argv,
        cwd,
        h.saturating_sub(2).max(1),
        w.saturating_sub(2).max(1),
    )?;

    loop {
        session.pump();

        terminal.draw(|f| {
            let area = f.area();
            let block = Block::bordered().title("banto · embedded — F12 to detach");
            let content = block.inner(area);
            f.render_widget(block, area);
            f.render_widget(Paragraph::new(screen_to_text(session.screen())), content);

            let screen = session.screen();
            if !screen.hide_cursor() {
                let (cr, cc) = screen.cursor_position();
                let (x, y) = (content.x + cc, content.y + cr);
                if x < content.x + content.width && y < content.y + content.height {
                    f.set_cursor_position(Position::new(x, y));
                }
            }
        })?;

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if matches!(key.code, KeyCode::F(12)) {
                        break;
                    }
                    if let Some(key) = convert::convert_key(key) {
                        session.send_key(&key);
                    }
                }
                Event::Resize(nw, nh) => {
                    session.resize(nh.saturating_sub(2).max(1), nw.saturating_sub(2).max(1));
                }
                _ => {}
            }
        }
    }
    Ok(())
}
