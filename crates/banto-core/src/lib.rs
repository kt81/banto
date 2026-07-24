//! banto-core: the pure constitution (`docs/DISCIPLINE.md` §2) — no I/O of
//! any kind, enforced by dependency (`crossterm`/`rusqlite`/`notify`/
//! `sysinfo`/`portable-pty`/`dirs` are all forbidden here; see each
//! submodule's doc for what its I/O counterpart is and where it lives).
//!
//! Module map (see docs/REQUIREMENTS.md for the design):
//! - [`model`]  shared domain types (pure data)
//! - [`input`]  pure key/mouse/paste/resize event types (no terminal backend)
//! - [`config`] config.toml's types (loading them from disk is `banto_io::config`)
//! - [`status`] activity age-bucketing math (live-process state is `banto_io::status`)
//! - [`search`] nucleo fuzzy search
//! - [`app`]    the classic list TUI's UI-free state (`App` and friends)
//! - [`engine`] the emporium's pure core: `update(state, ev, now) -> Vec<Cmd>`
//! - [`screen`] the emporium's per-pane `vt100` terminal model
//! - [`key_encode`] key-event -> PTY-child-stdin byte encoding
//! - [`replay`] the record/replay event-stream format (`docs/DISCIPLINE.md` §8)

pub mod app;
pub mod config;
pub mod engine;
pub mod input;
pub mod key_encode;
pub mod model;
pub mod replay;
pub mod screen;
pub mod search;
pub mod status;
