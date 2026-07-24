//! banto-core: UI-free logic for banto (session indexing, search, status).
//!
//! Module map (see docs/REQUIREMENTS.md for the design):
//! - [`model`]    shared domain types (frozen; changes go through the team lead)
//! - [`input`]    pure key/mouse/paste/resize event types (no terminal backend)
//! - [`provider`] session discovery + tolerant JSONL parsing
//! - [`status`]   live-session state and activity bucketing
//! - [`store`]    sqlite cache, FTS5, groups/pins, session<->pane map
//! - [`search`]   nucleo fuzzy search
//! - [`config`]   config.toml loading and default paths

pub mod config;
pub mod input;
pub mod model;
pub mod opener;
pub mod provider;
pub mod search;
pub mod status;
pub mod store;
pub mod watch;
