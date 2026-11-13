//! banto-tui: rendering from `&banto_core::app::App` / `&banto_core::engine`
//! state. Pure `(frame, state, area)` widgets — no key handling, no event
//! loop, no terminal setup; that stays in `banto` (bin), which owns raw
//! crossterm and drives the UI (the session list and the hosted panes).
//!
//! Module map:
//! - [`render_modal`] rendering `Modal` as a centered overlay
//! - [`paint`] `vt100` screen -> ratatui buffer cells (the emporium's panes)
//! - [`view`] the shared session-list / summary panel widgets
//! - [`strip`] the body of a collapsed stack tile, from a plain data struct
//! - [`hints`] the emporium's status-bar key hints, as text
//! - [`text`] column-aware text truncation, shared by `render_modal`/`view`

pub mod hints;
pub mod paint;
pub mod render_modal;
pub mod strip;
pub mod text;
pub mod view;
