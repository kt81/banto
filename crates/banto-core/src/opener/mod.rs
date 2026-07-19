//! Opening / focusing sessions in a real terminal (psmux, Windows Terminal).
//!
//! Design contract (docs/REQUIREMENTS.md "Opener spec", docs/notes/psmux-spike.md):
//! - Every external process invocation goes through an abstraction (trait)
//!   that unit tests mock; tests never spawn real processes.
//! - Backend priority: psmux (tmux-compatible CLI) first, Windows Terminal
//!   tab as fallback. Auto detection checks `$TMUX` before `WT_SESSION`.
//! - psmux pane user options are unusable; tag panes with `select-pane -T`
//!   and rely on the store's pane map as the source of truth.
//! - The resume command line and `banto _wrap` are built by the bin crate;
//!   this module receives a ready-made command + cwd.
//!
//! TODO(teammate): implement.
