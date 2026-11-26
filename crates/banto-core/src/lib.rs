//! banto-core: the pure constitution (`docs/DISCIPLINE.md` §2) — no I/O of
//! any kind, enforced by dependency (`crossterm`/`rusqlite`/`notify`/
//! `sysinfo`/`portable-pty`/`dirs` are all forbidden here; see each
//! submodule's doc for what its I/O counterpart is and where it lives).
//!
//! Module map (see docs/REQUIREMENTS.md for the design):
//! - [`model`]  shared domain types (pure data)
//! - [`input`]  pure key/mouse/paste/resize event types (no terminal backend)
//! - [`codex_model`] resolving a Codex model family word to a catalog slug (the
//!   catalog itself is read by `banto_io::codex_catalog`)
//! - [`codex_queue`] reading what `codex queue` answered into the outcomes the engine can act on (the process is
//!   run by `banto_io::codex_queue`)
//! - [`config`] config.toml's types (loading them from disk is `banto_io::config`)
//! - [`status`] activity age-bucketing math (live-process state is `banto_io::status`)
//! - [`search`] nucleo fuzzy search
//! - [`app`]    the session list's UI-free state (`App` and friends)
//! - [`engine`] the emporium's pure core: `update(state, ev, now) -> Vec<Cmd>`
//! - [`layout`] the emporium's geometry: tiles, drag handles and the layout
//!   preferences, as a pure function of the area, the stage and those
//!   preferences
//! - [`display_text`] the one policy for someone else's text shown on a banto line
//! - [`member_name`] the rule for a brigade member's display name
//! - [`consultation`] the rule for a Goinkyo consultation's title, and how the list shows it
//! - [`progress`] a Director's Progress and a decision's question, and the rule that accepts a write
//! - [`reading`] the F5 reading screen: its state, geometry and wrapped lines
//! - [`screen`] the emporium's per-pane `vt100` terminal model
//! - [`key_encode`] key-event -> PTY-child-stdin byte encoding
//! - [`replay`] the record/replay event-stream format (`docs/DISCIPLINE.md` §8)

pub mod app;
pub mod codex_model;
pub mod codex_queue;
pub mod config;
pub mod consultation;
pub mod display_text;
pub mod engine;
pub mod input;
pub mod key_encode;
pub mod layout;
pub mod member_name;
pub mod model;
pub mod pane_log;
pub mod plan;
pub mod program_state;
pub mod progress;
pub mod reading;
pub mod replay;
pub mod screen;
pub mod search;
pub mod status;
