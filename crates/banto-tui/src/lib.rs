//! banto-tui: rendering from `&banto_core::app::App` / `&banto_core::engine`
//! state. Pure `(frame, state, area)` widgets — no key handling, no event
//! loop, no terminal setup; that stays in `banto` (bin), which owns raw
//! crossterm and drives both UI modes (the classic list, the emporium).
//!
//! Module map:
//! - [`render_modal`] rendering `Modal` as a centered overlay — shared by
//!   both modes; `banto::tui` (the classic list, otherwise un-migrated by
//!   design) imports it back rather than rendering modals independently
//! - [`render`] `vt100` screen -> ratatui text (the emporium's embedded panes)
//! - [`view`] the shared session-list / summary panel widgets
//! - [`text`] column-aware text truncation, shared by `render_modal`/`view`

pub mod render;
pub mod render_modal;
pub mod text;
pub mod view;
