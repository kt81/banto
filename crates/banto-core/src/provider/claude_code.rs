//! Claude Code CLI session provider.
//!
//! Discovers `<claude_home>/projects/<encoded-cwd>/<uuid>.jsonl` files and
//! extracts metadata (title, cwd, mtime) by reading only the head of each
//! file. See docs/REQUIREMENTS.md "データソース" for the observed format.
//!
//! TODO(provider agent): implement.
