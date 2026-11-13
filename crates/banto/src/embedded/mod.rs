//! Embedded terminal host: banto's UI, which runs children (e.g. `claude --resume
//! <id>`) inside its own ratatui TUI — spawn each in a PTY, parse its output
//! with `vt100`, render the grid, and forward input. It is the productionised
//! form of the `docs/notes/embedded-pty-spike.md` case-A spike (banto as a
//! minimal multiplexer).
//!
//! Windows/ConPTY note (from the spike): banto must NOT answer the child's
//! DSR/DA queries — ConPTY is the child's terminal and passes host input
//! straight to the child's stdin, so answering leaks the reply as typed input.
//! A raw Unix PTY would need answering; that path is deferred.

mod convert;
mod emporium;
mod paste_accum;
mod session;

pub use emporium::EmporiumSettings;
pub use emporium::run as run_emporium;
#[cfg(test)]
pub(crate) use emporium::{brigade_env, mcp_config_json};
