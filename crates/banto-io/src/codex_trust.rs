//! Advisory-only reading of whether Codex has already been asked to trust
//! banto's own `SessionStart` hook (`crate::codex_home::CodexHome::
//! config_path`, `docs/notes/codex-briefing-spike.md` for the hash-of-
//! command-string trust mechanism this reads).
//!
//! Read-only, same as every other access under a product home: banto never
//! writes to `~/.codex` (invariant 1). A separate `banto codex-trust`
//! subcommand is what actually earns trust, by running the hook once under
//! Codex's own supervision; this module only reports whether that step
//! looks like it has already happened, so the operator can be told "this
//! looks unprimed" instead of finding out only when a briefing silently
//! never arrives.
//!
//! # Why this can only ever be a hint, never a guarantee
//!
//! 1. **The recorded hash cannot be verified.** Codex stores `trusted_hash =
//!    sha256(<its own normalized hook id>)`. Confirming that hash matches the
//!    hook command banto is using *today* would mean reimplementing Codex's
//!    own normalization outside Codex — reasoning about the meaning of an
//!    internal record banto does not own the format of. A hash left over
//!    from an old, differently-shaped hook command still reads as a match
//!    here, so a [`HookTrustState::Primed`] result can be a false positive.
//! 2. **The key's shape is provisional upstream, by Codex's own admission.**
//!    Its trust key is `<key_source>:<event>:<group_index>:<handler_index>`,
//!    a positional suffix Codex's own source flags as due for replacement
//!    with a durable hook id — a future Codex version can change this shape
//!    without banto knowing.
//!
//! The fact banto can actually stand behind is whether a briefing reached a
//! member, which is `banto_core::model::BrigadeMember::briefed_at` —
//! recorded by `banto _hook` itself when it runs, not guessed at from
//! Codex's own config. This module exists only to give the operator a
//! heads-up before that ever has a chance to matter.

use crate::codex_home::CodexHome;

/// See the module doc for why this is advisory, not authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookTrustState {
    /// A trust record whose key names banto's `-c`-synthesized hook source
    /// carries a non-empty, non-disabled `trusted_hash`.
    Primed,
    /// `config.toml` read and parsed without trouble, but no such record was
    /// found.
    NotPrimed,
    /// Nothing could be determined — the file doesn't exist, couldn't be
    /// read, or isn't valid TOML. Deliberately not an error: a caller
    /// prompting the operator to run `banto codex-trust` treats this the
    /// same as [`Self::NotPrimed`] (offer it), it just can't say why.
    Unknown,
}

/// The literal path segment Codex's `-c` override synthesizes a hook's
/// config source under — `C:\<session-flags>\config.toml` on Windows, a
/// differently-rooted path on other platforms (`docs/notes/codex-briefing-
/// spike.md`). Matched as an independent substring rather than reconstructed
/// into a full path: the full path is platform-dependent, and re-deriving it
/// here would be exactly the kind of guess that breaks the moment another
/// platform's root differs.
const SESSION_FLAGS_MARKER: &str = "<session-flags>";

/// The hook event banto's brigade hook is trusted under.
const SESSION_START_MARKER: &str = ":session_start:";

/// Reads `<codex_home>/config.toml` and reports whether it looks like
/// banto's own hook has already been trusted. Never fails — every read or
/// parse error degrades to [`HookTrustState::Unknown`].
pub fn hook_trust_state(codex_home: &CodexHome) -> HookTrustState {
    let Ok(text) = std::fs::read_to_string(codex_home.config_path()) else {
        return HookTrustState::Unknown;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return HookTrustState::Unknown;
    };
    hook_trust_state_from_value(&value)
}

/// The parsing/matching core of [`hook_trust_state`], split out so tests can
/// exercise it directly against synthetic TOML without touching a filesystem.
fn hook_trust_state_from_value(value: &toml::Value) -> HookTrustState {
    let state_table = value
        .as_table()
        .and_then(|top| top.get("hooks"))
        .and_then(toml::Value::as_table)
        .and_then(|hooks| hooks.get("state"))
        .and_then(toml::Value::as_table);

    let Some(state_table) = state_table else {
        return HookTrustState::NotPrimed;
    };

    let primed = state_table.iter().any(|(key, record)| {
        if !key.contains(SESSION_FLAGS_MARKER) || !key.contains(SESSION_START_MARKER) {
            return false;
        }
        let Some(record) = record.as_table() else {
            return false;
        };
        let has_hash = record
            .get("trusted_hash")
            .and_then(toml::Value::as_str)
            .is_some_and(|hash| !hash.is_empty());
        let not_disabled = record
            .get("enabled")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        has_hash && not_disabled
    });

    if primed {
        HookTrustState::Primed
    } else {
        HookTrustState::NotPrimed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_from(text: &str) -> HookTrustState {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), text).unwrap();
        hook_trust_state(&CodexHome::new(dir.path().to_path_buf()))
    }

    #[test]
    fn a_real_shaped_windows_key_with_a_hash_is_primed() {
        let text = r#"
[hooks.state]

[hooks.state.'C:\<session-flags>\config.toml:session_start:0:0']
trusted_hash = "sha256:a751b947abc123"
enabled = true
"#;
        assert_eq!(state_from(text), HookTrustState::Primed);
    }

    #[test]
    fn a_non_windows_shaped_key_is_also_primed() {
        let text = r#"
[hooks.state.'/synthetic-root/<session-flags>/config.toml:session_start:0:0']
trusted_hash = "sha256:a751b947abc123"
enabled = true
"#;
        assert_eq!(state_from(text), HookTrustState::Primed);
    }

    #[test]
    fn an_empty_hooks_state_table_is_not_primed() {
        assert_eq!(state_from("[hooks.state]\n"), HookTrustState::NotPrimed);
    }

    #[test]
    fn a_missing_hooks_section_entirely_is_not_primed() {
        assert_eq!(state_from("workers = 1\n"), HookTrustState::NotPrimed);
    }

    #[test]
    fn a_missing_config_file_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        // config.toml is never created under dir.
        let state = hook_trust_state(&CodexHome::new(dir.path().to_path_buf()));
        assert_eq!(state, HookTrustState::Unknown);
    }

    #[test]
    fn broken_toml_is_unknown() {
        assert_eq!(
            state_from("this is not [valid toml"),
            HookTrustState::Unknown
        );
    }

    #[test]
    fn an_empty_trusted_hash_is_not_primed() {
        let text = r#"
[hooks.state.'C:\<session-flags>\config.toml:session_start:0:0']
trusted_hash = ""
enabled = true
"#;
        assert_eq!(state_from(text), HookTrustState::NotPrimed);
    }

    #[test]
    fn a_disabled_record_with_a_hash_is_not_primed() {
        let text = r#"
[hooks.state.'C:\<session-flags>\config.toml:session_start:0:0']
trusted_hash = "sha256:a751b947abc123"
enabled = false
"#;
        assert_eq!(state_from(text), HookTrustState::NotPrimed);
    }

    #[test]
    fn a_key_naming_a_different_event_is_ignored() {
        // Same synthetic source, a different hook event entirely — must not
        // be mistaken for this brigade's SessionStart trust.
        let text = r#"
[hooks.state.'C:\<session-flags>\config.toml:pre_tool_use:0:0']
trusted_hash = "sha256:a751b947abc123"
enabled = true
"#;
        assert_eq!(state_from(text), HookTrustState::NotPrimed);
    }

    #[test]
    fn a_real_looking_operator_hook_key_without_the_synthetic_marker_is_ignored() {
        // An operator's own hook, keyed under a real path rather than the
        // `-c` layer's synthetic one — must not be mistaken for banto's.
        let text = r#"
[hooks.state.'C:\Users\me\.codex\config.toml:session_start:0:0']
trusted_hash = "sha256:a751b947abc123"
enabled = true
"#;
        assert_eq!(state_from(text), HookTrustState::NotPrimed);
    }
}
