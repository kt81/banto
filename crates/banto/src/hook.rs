//! `banto _hook`: the Codex `SessionStart` hook process banto's own launch
//! argv wires in for a Codex brigade member (`docs/notes/codex-briefing-
//! spike.md` — Codex has no `--append-system-prompt`, so the briefing a
//! Claude member gets on its launch argv has to reach a Codex member some
//! other way). Reads the hook's JSON on stdin, prints
//! `{"hookSpecificOutput": {...}}` on stdout, and records in banto's own
//! store that the hook fired for this member — see
//! `banto_core::model::BrigadeMember::briefed_at`'s doc for why that record
//! has to exist at all (every gate the spike measured that can eat a
//! briefing fails silently).
//!
//! The briefing is not printed on every firing. Codex keeps each copy a hook
//! returns and replays all of them on resume, so answering every firing piled
//! identical copies into a member's history. On a `resume` the hook therefore
//! reads the session's rollout — the one agent file it touches, read-only, at the
//! path Codex puts in its own stdin — and prints `{}` when the most recent
//! thing the rollout says about the briefing is a copy of this exact text;
//! every other firing, and every doubt, prints it. `store.record_briefing`
//! runs either way: a withheld briefing still reached the member. The
//! decision and its grounds are `banto_io::codex_briefing`'s.
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
//! own strict config load, specifically so a broken `--config`/`BANTO_
//! CONFIG` the operator has set for unrelated reasons can't fail the hook
//! either — this module resolves and loads its own config independently,
//! always leniently, even for an explicit override that `main.rs`'s own
//! resolution would treat as a hard error.
//!
//! Where the config file and the database are comes from the banto that
//! launched the agent, through the environment (`BANTO_PARENT_CONFIG`,
//! `BANTO_PARENT_NO_CONFIG`, `BANTO_PARENT_DB`) for the same reason identity
//! does: the command line can carry nothing. Without them (an older banto
//! launched this agent) the hook resolves both for itself, as it always did.

use std::io::{Read, Write};
use std::path::Path;
use std::time::SystemTime;

use serde_json::{Value, json};

use banto_core::config::Config;
use banto_core::model::{AgentKind, BrigadeId, BrigadeRole, SessionId};
use banto_io::codex_briefing::{self, Verdict};
use banto_io::store::Store;

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
        role: get("BANTO_ROLE")
            .as_deref()
            .and_then(BrigadeRole::from_token),
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
/// this module's doc), and `crate::briefing::compose` appends the
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
    let peers = crate::briefing::peers_of(store, brigade_id, role);
    // No request path: a Goinkyo is Claude-only, so this hook — Codex's own
    // SessionStart briefing path — never renders one.
    crate::briefing::compose(
        &config.brigade,
        AgentKind::Codex,
        role,
        brigade_id,
        token,
        &peers,
        None,
    )
}

/// Resolves banto's own config and database, always leniently — see the module
/// doc for why this deliberately doesn't use the strict load `main.rs` does for
/// the other commands (it fails outright on a broken explicit override). `None`
/// for the store on any failure (unresolvable path, unopenable file): never an
/// error the hook has to propagate.
fn load_leniently() -> (Config, Option<Store>) {
    let mut inputs = crate::startup::Inputs {
        env: crate::startup::Env::from_process(),
        cwd: std::env::current_dir().ok(),
        ..Default::default()
    };
    // The parent banto's own answer, when it gave one: the config file and
    // database it resolved, through the environment because this command line
    // can carry nothing. Absent (a hook an older banto launched), the hook
    // resolves for itself.
    inputs.apply_parent_env(|key| std::env::var(key).ok());
    let Ok((config, resolved)) =
        crate::startup::resolve(&inputs, crate::startup::Strictness::Lenient)
    else {
        return (Config::default(), None);
    };
    (config, open_store_leniently(&resolved))
}

/// The store, or `None` when there is none to open — no database location, an
/// empty one (refused by [`crate::startup::Resolved::db_path`], so nothing is
/// opened or created), or a file that will not open. The hook then runs exactly
/// as it does without a store: no briefing, nothing recorded, `{}` on stdout.
/// That includes a database at a schema other than this build's: the hook never
/// migrates (`Store::open_current`), so a banto replaced on disk under a running
/// UI briefs nobody rather than upgrading the database beneath it.
fn open_store_leniently(resolved: &crate::startup::Resolved) -> Option<Store> {
    Store::open_current(resolved.db_path().ok()?).ok()
}

/// Entry point for `banto _hook`. Always exits 0 (falls off the end of
/// `main`'s `Command::Hook` arm) and always prints one line of valid JSON.
pub fn run() {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let stdin_json: Value = serde_json::from_str(&input).unwrap_or(Value::Null);

    let identity = read_identity(|key| std::env::var(key).ok());
    let (config, store) = load_leniently();

    let output = fire(
        &stdin_json,
        &identity,
        &config,
        store.as_ref(),
        SystemTime::now(),
    );
    let _ = writeln!(std::io::stdout(), "{output}");
}

/// Everything one firing does once its inputs are in hand: record that the hook
/// fired, and say what to print. [`run`] is only the reading of stdin, the
/// environment and the store around it, which is what keeps the recording
/// testable without a process.
fn fire(
    stdin: &Value,
    identity: &HookIdentity,
    config: &Config,
    store: Option<&Store>,
    now: SystemTime,
) -> Value {
    let briefing = build_briefing(identity, config, store);

    // Recorded independent of whether `briefing` is `Some`, and of whether it is
    // then printed: a role briefed with an intentionally-empty template, or a
    // resume whose history already holds the briefing, still had the hook reach
    // it, which is the fact `briefed_at` exists to prove — see this module's
    // doc and `BrigadeMember::briefed_at`'s. What it does need is a store, a
    // complete identity and a `session_id` on stdin.
    if let (Some(store), Some(brigade_id), Some(token), Some(session_id)) = (
        store,
        identity.brigade_id,
        identity.token.as_deref(),
        session_id_from_stdin(stdin),
    ) {
        let _ = store.record_briefing(brigade_id, token, &session_id, now);
    }

    hook_output(briefing, stdin)
}

/// What the hook prints. `{}` when there is no briefing, and also when the
/// session already holds this exact one — see [`banto_io::codex_briefing`] for
/// why Codex needs that said, and for what counts as proof. Whatever stdin
/// does not say (`source`, `transcript_path`) is doubt, and doubt delivers.
///
/// The recording in [`fire`] does not depend on this: a firing whose briefing is
/// withheld still reached the member, which is what `briefed_at` exists to say.
fn hook_output(briefing: Option<String>, stdin: &Value) -> Value {
    let Some(additional_context) = briefing else {
        return json!({});
    };
    let source = stdin.get("source").and_then(Value::as_str);
    let transcript_path = stdin
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(Path::new);
    if codex_briefing::verdict(source, transcript_path, &additional_context) == Verdict::Suppress {
        return json!({});
    }
    json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": additional_context,
        }
    })
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

    /// One hook copy of `text` as a rollout line (synthetic; the shape is the
    /// measured one — see `banto_io::codex_briefing`'s tests).
    fn rollout_copy(text: &str) -> String {
        json!({"type": "response_item", "payload": {"type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": text}]}})
        .to_string()
    }

    fn rollout_file(lines: &[String]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut text = lines.join("\n");
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        let path = path.to_str().unwrap().to_string();
        (dir, path)
    }

    fn delivered(output: &Value) -> Option<&str> {
        output["hookSpecificOutput"]["additionalContext"].as_str()
    }

    #[test]
    fn a_resume_whose_history_holds_the_briefing_prints_nothing() {
        let (_dir, path) = rollout_file(&[rollout_copy("the briefing")]);
        let stdin = json!({"source": "resume", "transcript_path": path});
        assert_eq!(
            hook_output(Some("the briefing".to_string()), &stdin),
            json!({})
        );
    }

    #[test]
    fn a_resume_whose_history_lacks_the_briefing_delivers_it() {
        let (_dir, path) = rollout_file(&[rollout_copy("some other text")]);
        let stdin = json!({"source": "resume", "transcript_path": path});
        let output = hook_output(Some("the briefing".to_string()), &stdin);
        assert_eq!(delivered(&output), Some("the briefing"));
        assert_eq!(
            output["hookSpecificOutput"]["hookEventName"],
            "SessionStart"
        );
    }

    #[test]
    fn every_firing_that_is_not_a_proven_resume_delivers() {
        let (_dir, path) = rollout_file(&[rollout_copy("the briefing")]);
        for stdin in [
            json!({"source": "startup", "transcript_path": path}),
            json!({"source": "compact", "transcript_path": path}),
            json!({"source": "clear", "transcript_path": path}),
            json!({"source": "fork", "transcript_path": path}),
            json!({"source": "resume"}),
            json!({"transcript_path": path}),
            json!({"source": 7, "transcript_path": path}),
            json!({"source": "resume", "transcript_path": 7}),
            json!({"source": "resume", "transcript_path": "/no/such/rollout.jsonl"}),
            Value::Null,
        ] {
            let output = hook_output(Some("the briefing".to_string()), &stdin);
            assert_eq!(delivered(&output), Some("the briefing"), "stdin {stdin}");
        }
    }

    #[test]
    fn no_briefing_prints_nothing_whatever_stdin_says() {
        let (_dir, path) = rollout_file(&[rollout_copy("the briefing")]);
        let stdin = json!({"source": "startup", "transcript_path": path});
        assert_eq!(hook_output(None, &stdin), json!({}));
    }

    /// A brigade with a Director and one Worker, and the identity of that Worker.
    fn worker_in_a_cell() -> (Store, HookIdentity) {
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
        (store, identity)
    }

    #[test]
    fn a_withheld_firing_still_records_that_the_hook_fired() {
        let (store, identity) = worker_in_a_cell();
        let config = brigade_config("I am {token}");
        let briefing = build_briefing(&identity, &config, Some(&store)).unwrap();
        let (_dir, path) = rollout_file(&[rollout_copy(&briefing)]);
        let stdin = json!({"session_id": "s-1", "source": "resume", "transcript_path": path});
        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(5_000);

        let output = fire(&stdin, &identity, &config, Some(&store), at);

        assert_eq!(output, json!({}), "the history holds it: nothing printed");
        let member = store
            .brigade_member(identity.brigade_id.unwrap(), "worker-1")
            .unwrap()
            .unwrap();
        assert_eq!(member.briefed_at, Some(at));
        assert_eq!(
            member.briefed_session_id,
            Some(SessionId("s-1".to_string()))
        );
    }

    #[test]
    fn a_delivered_firing_records_too() {
        let (store, identity) = worker_in_a_cell();
        let config = brigade_config("I am {token}");
        let stdin = json!({"session_id": "s-1", "source": "startup"});
        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(5_000);

        let output = fire(&stdin, &identity, &config, Some(&store), at);

        assert!(delivered(&output).is_some());
        let member = store
            .brigade_member(identity.brigade_id.unwrap(), "worker-1")
            .unwrap()
            .unwrap();
        assert_eq!(member.briefed_at, Some(at));
    }

    #[test]
    fn nothing_is_recorded_without_a_session_id_on_stdin() {
        // Withheld or not, `briefed_at` needs the session the hook ran under.
        let (store, identity) = worker_in_a_cell();
        let config = brigade_config("I am {token}");
        let stdin = json!({"source": "startup"});

        let output = fire(&stdin, &identity, &config, Some(&store), SystemTime::now());

        assert!(delivered(&output).is_some());
        let member = store
            .brigade_member(identity.brigade_id.unwrap(), "worker-1")
            .unwrap()
            .unwrap();
        assert_eq!(member.briefed_at, None);
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
            Some(crate::briefing::with_codex_addendum(
                &format!("I am worker-1 in {brigade_id}, peers: director"),
                BrigadeRole::Worker,
            ))
        );
    }

    #[test]
    fn a_codex_director_briefing_carries_the_consultation_level_before_the_addendum() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade_id = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(brigade_id, "director", BrigadeRole::Director, None)
            .unwrap();
        let identity = HookIdentity {
            brigade_id: Some(brigade_id),
            token: Some("director".to_string()),
            role: Some(BrigadeRole::Director),
        };
        let mut config = brigade_config("I lead {brigade}");
        config.brigade.goinkyo_consult = "disputes".to_string();

        let briefing = build_briefing(&identity, &config, Some(&store)).unwrap();
        let paragraph =
            crate::briefing::consult_paragraph(banto_core::config::GoinkyoConsult::Disputes);
        assert!(
            briefing.starts_with(&format!("I lead {brigade_id}\n\n{paragraph}\n\n")),
            "{briefing}"
        );
        assert!(briefing.ends_with(codex_briefing::BRIEFING_LAST_SENTENCE));

        // The same level on the same config, as the Claude launch path renders it.
        let claude = crate::briefing::compose(
            &config.brigade,
            AgentKind::ClaudeCode,
            BrigadeRole::Director,
            brigade_id,
            "director",
            &[],
            None,
        )
        .unwrap();
        assert_eq!(claude, format!("I lead {brigade_id}\n\n{paragraph}"));

        // A Worker in the same cell is never given it.
        let worker = HookIdentity {
            brigade_id: Some(brigade_id),
            token: Some("worker-1".to_string()),
            role: Some(BrigadeRole::Worker),
        };
        assert!(
            !build_briefing(&worker, &config, Some(&store))
                .unwrap()
                .contains("When to consult the Goinkyo")
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
            Some(crate::briefing::with_codex_addendum(
                "",
                BrigadeRole::Worker
            ))
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

    fn resolved_with_db(db_path: Option<std::path::PathBuf>) -> crate::startup::Resolved {
        crate::startup::Resolved {
            config_file: crate::startup::ConfigFile::None,
            db_path,
            claude_home: None,
            codex_home: None,
            data_dir: None,
        }
    }

    /// An agent's home is read-only for banto: a hook whose database path was
    /// pointed into one opens nothing there, so it creates nothing in it either.
    #[test]
    fn a_database_path_inside_an_agent_home_means_no_store_and_nothing_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("claude-home");
        std::fs::create_dir_all(&home).unwrap();
        let mut resolved = resolved_with_db(Some(home.join("banto.db")));
        resolved.claude_home = Some(home.clone());
        assert!(open_store_leniently(&resolved).is_none());
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
        // Beside the home is an ordinary place for it.
        resolved.db_path = Some(dir.path().join("banto.db"));
        drop(Store::open(resolved.db_path.as_ref().unwrap()).unwrap());
        assert!(open_store_leniently(&resolved).is_some());
    }

    #[test]
    fn an_empty_database_path_means_no_store_so_no_briefing_and_nothing_is_created() {
        let dir = tempfile::tempdir().unwrap();
        // The hook stays lenient: no store, never an error.
        assert!(open_store_leniently(&resolved_with_db(Some(std::path::PathBuf::new()))).is_none());
        assert!(open_store_leniently(&resolved_with_db(None)).is_none());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        // With no store the briefing is `None`, i.e. the hook prints `{}` and
        // records nothing (`build_briefing_is_none_without_a_store`).
        let identity = HookIdentity {
            brigade_id: Some(1),
            token: Some("worker-1".to_string()),
            role: Some(BrigadeRole::Worker),
        };
        assert_eq!(build_briefing(&identity, &Config::default(), None), None);
    }

    #[test]
    fn a_current_database_still_opens_a_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("banto.db");
        drop(Store::open(&path).unwrap());
        assert!(open_store_leniently(&resolved_with_db(Some(path))).is_some());
    }

    /// The hook is a child of the agent a UI started, never the one that creates
    /// the database: with none there is no store, and nothing appears.
    #[test]
    fn a_database_that_does_not_exist_is_not_created_by_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("banto.db");
        assert!(open_store_leniently(&resolved_with_db(Some(path.clone()))).is_none());
        assert!(!path.exists());
    }

    /// A banto replaced on disk under a running UI: this build's hook must not
    /// migrate the database beneath that UI. No store, so no briefing, exit 0.
    #[test]
    fn a_database_at_another_schema_is_left_alone_and_means_no_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("banto.db");
        drop(Store::open(&path).unwrap());
        let older = banto_io::store::SCHEMA_VERSION - 1;
        rusqlite::Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", older)
            .unwrap();

        assert!(open_store_leniently(&resolved_with_db(Some(path.clone()))).is_none());

        let version: i64 = rusqlite::Connection::open(&path)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, older);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no copy");
    }
}
