//! Persistent store: sqlite (rusqlite, bundled) under dirs::data_local_dir/banto.
//!
//! Responsibilities (docs/REQUIREMENTS.md "モジュール構成"):
//! - session index cache (metadata mirror of provider discovery) + FTS5 text index
//! - groups / pins (banto-owned; never written to Claude's files)
//! - session <-> pane mapping for the opener (phase 2 consumes this)
//!
//! TODO(store agent): implement.
