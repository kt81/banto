//! `banto _hook`: the Codex `SessionStart` hook process banto's own launch
//! argv wires in for a Codex brigade member (`docs/notes/codex-briefing-
//! spike.md` — Codex has no `--append-system-prompt`, so the briefing a
//! Claude member gets on its launch argv has to reach a Codex member some
//! other way). Reads the hook's JSON on stdin, prints
//! `{"hookSpecificOutput": {...}}` on stdout, and records in banto's own
//! store that the briefing reached this member — see
//! `banto_core::model::BrigadeMember::briefed_at`'s doc for why that record
//! has to exist at all (every gate the spike measured that can eat a
//! briefing fails silently).
//!
//! Takes no CLI arguments, on purpose, forever: Codex trusts a hook by
//! hashing its literal command string, and silently refuses to run one whose
//! command string no longer matches what was trusted (measured empirically —
//! no error, no warning, the hook just never fires). Adding so much as a
//! flag here changes that string and breaks trust for every brigade launch
//! site already approved, silently, for everyone who upgrades. Member
//! identity travels through `BANTO_BRIGADE`/`BANTO_MEMBER`/`BANTO_ROLE`
//! instead, which the hook process inherits from its environment — see
//! `crate::embedded::emporium`'s `brigade_env` for where those are set.
//!
//! This process interrupts the *operator's own* Codex session's own startup
//! path, for a feature (the brigade briefing) the operator may not even be
//! using. Nothing here may ever crash or block: every failure (missing env,
//! a store that won't open, an empty or disabled briefing template,
//! malformed stdin) degrades to "no briefing", never to a non-zero exit or a
//! hung process. `main.rs` dispatches `Command::Hook` to [`run`] before its
//! own `load_config(...)?`, specifically so a broken `--config`/`BANTO_
//! CONFIG` the operator has set for unrelated reasons can't fail the hook
//! either — this module resolves and loads its own config independently,
//! always leniently, even for an explicit override that `main.rs`'s own
//! resolution would treat as a hard error.

use std::io::{Read, Write};
use std::time::SystemTime;

use serde_json::{Value, json};

use banto_core::config::Config;
use banto_core::model::{BrigadeId, BrigadeRole, SessionId};
use banto_io::config as io_config;
use banto_io::store::Store;

use crate::mcp::parse_role;

/// Member identity as read from the environment. Every field is optional
/// because the whole point of this module is to degrade to "no briefing"
/// rather than fail when any of it is missing or unparsable.
struct HookIdentity {
    brigade_id: Option<BrigadeId>,
    token: Option<String>,
    role: Option<BrigadeRole>,
}

/// Reads [`HookIdentity`] via an injected getter (`std::env::var(key).ok()`
/// in production) so tests can supply synthetic environments without
/// mutating the real process environment.
fn read_identity(get: impl Fn(&str) -> Option<String>) -> HookIdentity {
    HookIdentity {
        brigade_id: get("BANTO_BRIGADE").and_then(|value| value.parse().ok()),
        token: get("BANTO_MEMBER"),
        role: get("BANTO_ROLE").as_deref().and_then(parse_role),
    }
}

/// Pulls `session_id` out of the hook's stdin JSON. Leniently: a missing
/// field, wrong type, or entirely unparsable stdin all just yield `None`.
fn session_id_from_stdin(stdin: &Value) -> Option<SessionId> {
    stdin
        .get("session_id")
        .and_then(Value::as_str)
        .map(|s| SessionId(s.to_string()))
}

/// This member's role briefing, or `None` when identity is incomplete or the
/// store couldn't be opened. Unlike `crate::embedded::emporium`'s
/// `member_briefing` (the Claude launch path, where an empty role template
/// means "no `--append-system-prompt` flag at all"), an empty template here
/// still yields `Some` — every caller of `banto _hook` is a Codex member (see
/// this module's doc), and `crate::briefing::with_codex_addendum` appends the
/// Codex-only facts regardless of whether the operator gave this role a
/// template, so the empty-template escape hatch only ever suppresses the
/// role-specific sentence, never the addendum.
fn build_briefing(
    identity: &HookIdentity,
    config: &Config,
    store: Option<&Store>,
) -> Option<String> {
    let brigade_id = identity.brigade_id?;
    let token = identity.token.as_deref()?;
    let role = identity.role?;
    let store = store?;
    let template = config.brigade.prompt_for(role).unwrap_or("");
    let peers = crate::briefing::peers_of(store, brigade_id, role);
    let rendered = crate::briefing::render(template, brigade_id, token, &peers);
    Some(crate::briefing::with_codex_addendum(&rendered))
}

/// Resolves and loads banto's own config, always leniently — see the module
/// doc for why this deliberately doesn't reuse `main.rs`'s stricter
/// `load_config` (which fails outright on a broken explicit override).
fn load_config_leniently() -> Config {
    let env_override = std::env::var_os("BANTO_CONFIG").map(std::path::PathBuf::from);
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from);
    let home = dirs::home_dir();

    let path = match io_config::resolve_config_path(
        None,
        env_override.as_deref(),
        xdg_config_home.as_deref(),
        home.as_deref(),
    ) {
        io_config::ConfigSource::Explicit(path) => Some(path),
        io_config::ConfigSource::Discovered(path) => Some(path),
        io_config::ConfigSource::Default(path) => path,
    };

    path.map(|path| io_config::load_or_default(&path))
        .unwrap_or_default()
}

/// Opens banto's own store, or `None` on any failure (unresolvable path,
/// unopenable file) — never an error the hook has to propagate.
fn open_store_leniently(config: &Config) -> Option<Store> {
    let path = config.db_path.clone().or_else(io_config::default_db_path)?;
    Store::open(&path).ok()
}

/// Entry point for `banto _hook`. Always exits 0 (falls off the end of
/// `main`'s `Command::Hook` arm) and always prints one line of valid JSON.
pub fn run() {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let stdin_json: Value = serde_json::from_str(&input).unwrap_or(Value::Null);

    let identity = read_identity(|key| std::env::var(key).ok());
    let config = load_config_leniently();
    let store = open_store_leniently(&config);

    let briefing = build_briefing(&identity, &config, store.as_ref());

    // Recorded independent of whether `briefing` is `Some`: a role briefed
    // with an intentionally-empty template still had the hook reach it,
    // which is the fact `briefed_at` exists to prove — see this module's
    // doc and `BrigadeMember::briefed_at`'s.
    if let (Some(store), Some(brigade_id), Some(token), Some(session_id)) = (
        store.as_ref(),
        identity.brigade_id,
        identity.token.as_deref(),
        session_id_from_stdin(&stdin_json),
    ) {
        let _ = store.record_briefing(brigade_id, token, &session_id, SystemTime::now());
    }

    let output = match briefing {
        Some(additional_context) => json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": additional_context,
            }
        }),
        None => json!({}),
    };
    let _ = writeln!(std::io::stdout(), "{output}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::config::BrigadeConfig;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn read_identity_parses_a_complete_environment() {
        let identity = read_identity(env(&[
            ("BANTO_BRIGADE", "7"),
            ("BANTO_MEMBER", "worker-1"),
            ("BANTO_ROLE", "worker"),
        ]));
        assert_eq!(identity.brigade_id, Some(7));
        assert_eq!(identity.token.as_deref(), Some("worker-1"));
        assert_eq!(identity.role, Some(BrigadeRole::Worker));
    }

    #[test]
    fn read_identity_is_lenient_about_missing_or_malformed_variables() {
        let identity = read_identity(env(&[]));
        assert_eq!(identity.brigade_id, None);
        assert_eq!(identity.token, None);
        assert_eq!(identity.role, None);

        // A non-numeric BANTO_BRIGADE and an unrecognized BANTO_ROLE degrade
        // to None rather than panicking on the parse.
        let identity = read_identity(env(&[
            ("BANTO_BRIGADE", "not-a-number"),
            ("BANTO_ROLE", "sous-chef"),
        ]));
        assert_eq!(identity.brigade_id, None);
        assert_eq!(identity.role, None);
    }

    #[test]
    fn session_id_from_stdin_reads_the_measured_field() {
        let stdin = serde_json::json!({"session_id": "abc-123", "source": "resume"});
        assert_eq!(
            session_id_from_stdin(&stdin),
            Some(SessionId("abc-123".to_string()))
        );
    }

    #[test]
    fn session_id_from_stdin_is_lenient_about_missing_or_malformed_input() {
        assert_eq!(session_id_from_stdin(&Value::Null), None);
        assert_eq!(session_id_from_stdin(&serde_json::json!({})), None);
        assert_eq!(
            session_id_from_stdin(&serde_json::json!({"session_id": 123})),
            None
        );
    }

    fn brigade_config(worker_prompt: &str) -> Config {
        Config {
            brigade: BrigadeConfig {
                worker_prompt: worker_prompt.to_string(),
                director_prompt: worker_prompt.to_string(),
                ..BrigadeConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn build_briefing_renders_the_template_against_the_live_roster() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(brigade_id, "director", BrigadeRole::Director, None)
            .unwrap();
        store
            .add_brigade_member(brigade_id, "worker-1", BrigadeRole::Worker, None)
            .unwrap();

        let identity = HookIdentity {
            brigade_id: Some(brigade_id),
            token: Some("worker-1".to_string()),
            role: Some(BrigadeRole::Worker),
        };
        let config = brigade_config("I am {token} in {brigade}, peers: {peers}");

        let briefing = build_briefing(&identity, &config, Some(&store));
        assert_eq!(
            briefing,
            Some(crate::briefing::with_codex_addendum(&format!(
                "I am worker-1 in {brigade_id}, peers: director"
            )))
        );
    }

    #[test]
    fn build_briefing_still_carries_the_codex_addendum_when_the_role_template_is_disabled() {
        // The empty-string escape hatch suppresses the role sentence, not
        // the Codex-only facts every `banto _hook` caller needs regardless.
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(brigade_id, "worker-1", BrigadeRole::Worker, None)
            .unwrap();

        let identity = HookIdentity {
            brigade_id: Some(brigade_id),
            token: Some("worker-1".to_string()),
            role: Some(BrigadeRole::Worker),
        };
        let config = brigade_config("");

        assert_eq!(
            build_briefing(&identity, &config, Some(&store)),
            Some(crate::briefing::with_codex_addendum(""))
        );
    }

    #[test]
    fn build_briefing_is_none_without_a_store() {
        let identity = HookIdentity {
            brigade_id: Some(1),
            token: Some("worker-1".to_string()),
            role: Some(BrigadeRole::Worker),
        };
        let config = brigade_config("hello {token}");

        assert_eq!(build_briefing(&identity, &config, None), None);
    }

    #[test]
    fn build_briefing_is_none_with_incomplete_identity() {
        let store = Store::open_in_memory().unwrap();
        let config = brigade_config("hello {token}");

        let missing_brigade = HookIdentity {
            brigade_id: None,
            token: Some("worker-1".to_string()),
            role: Some(BrigadeRole::Worker),
        };
        assert_eq!(
            build_briefing(&missing_brigade, &config, Some(&store)),
            None
        );

        let missing_role = HookIdentity {
            brigade_id: Some(1),
            token: Some("worker-1".to_string()),
            role: None,
        };
        assert_eq!(build_briefing(&missing_role, &config, Some(&store)), None);
    }
}
