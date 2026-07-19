//! Filesystem watching for live TUI updates (phase 3).
//!
//! Watches `<claude_home>/projects/` and `<claude_home>/sessions/` with
//! `notify`, debouncing bursts into coarse change events. Debounce logic must
//! be pure and testable without a real filesystem watcher; the notify-backed
//! implementation sits behind an abstraction.
//!
//! TODO(teammate): implement.
