//! banto configuration types.
//!
//! Every field has a default and unknown keys are ignored (`#[serde(default)]`
//! throughout), so any subset of a TOML document deserializes into a valid
//! [`Config`] — that leniency is the contract these types promise; loading
//! the actual `config.toml` file (locating it, reading it, turning a parse
//! failure into an error or a silent default) is `banto_io::config`'s job —
//! it needs filesystem access and the `dirs` crate for the default path,
//! both forbidden here (`docs/DISCIPLINE.md` §2).

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::Deserialize;

use crate::model::{AgentKind, BrigadeRole};

/// Which backend resumes sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpenerMode {
    /// Take over banto's own pane: collapse the TUI and run the session as a
    /// child process in the same terminal, no terminal multiplexer involved.
    /// The default — split/tab placement (below) is reserved for the `s`
    /// key.
    #[default]
    InPlace,
    /// Detect a split/tab backend from the environment: `$TMUX` first
    /// (resolving to real `tmux`, or to `psmux` on Windows), then
    /// `WT_SESSION`.
    Auto,
    Psmux,
    /// Real `tmux`, as distinct from `psmux`: the two address panes
    /// differently (see `banto_io::opener::TmuxFlavor`), so which one is
    /// driving has to be known, not guessed.
    Tmux,
    WindowsTerminal,
}

/// Thresholds for the activity age buckets (plain numbers here; converted to
/// `banto_core::status::AgeThresholds` by `banto::session::thresholds_from`,
/// which feeds `status::age_bucket`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ActivityConfig {
    /// Sessions modified within this many hours count as "today".
    pub today_hours: u64,
    /// Sessions modified within this many days count as "this week".
    pub week_days: u64,
}

impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            today_hours: 24,
            week_days: 7,
        }
    }
}

/// Whether banto auto-nudges a brigade member's stdin once it has unseen
/// messages and is observed idle, or leaves delivery entirely to a human
/// prompting `check_messages` (`[brigade] relay` in config.toml).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RelayMode {
    /// banto nudges an idle member with unseen messages (see the emporium's
    /// relay engine).
    #[default]
    Auto,
    /// The relay engine is disabled entirely.
    Manual,
}

/// Lenient by design, unlike [`OpenerMode`] (which rejects an unrecognized
/// value with a parse error): an unrecognized `relay` string falls back to
/// [`RelayMode::Auto`] rather than failing the whole config load, since a
/// typo silently keeping the relay on is preferable to it silently taking
/// the rest of `config.toml` down with it.
impl<'de> Deserialize<'de> for RelayMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "manual" => RelayMode::Manual,
            _ => RelayMode::Auto,
        })
    }
}

/// Which product an auto-spawned Worker runs as, relative to the Director
/// forming the brigade — `[brigade] worker_agent` in config.toml.
///
/// Not yet consumed anywhere: `crate::engine::spawn_worker` still always
/// spawns a Claude Worker (`AgentKind::ClaudeCode` is hardcoded there). This
/// type is the config-layer half of letting a brigade's Worker be Codex too;
/// the formation logic that reads it, and the new-session-modal-style picker
/// [`Self::Select`] needs, land separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkerAgentSetting {
    /// Same product as the Director. The default: an operator who never
    /// heard of this setting keeps getting exactly what banto has always
    /// spawned for a Claude Director, and a Codex Director's Workers follow
    /// it symmetrically rather than silently defaulting to Claude anyway.
    #[default]
    Inherit,
    /// Ask at formation time (a picker, the same axis the new-session modal
    /// already offers for a plain session — decided in the UI layer, not
    /// here).
    Select,
    /// Always Claude Code, regardless of the Director's own product.
    Claude,
    /// Always Codex, regardless of the Director's own product.
    Codex,
}

impl WorkerAgentSetting {
    /// Parses `[brigade] worker_agent`'s raw string. Case-insensitive and
    /// whitespace-trimmed — the same leniency [`resolve_agents`] already
    /// applies to the neighboring `agents` setting, since both name the
    /// same product vocabulary an operator might type inconsistently — and
    /// an unrecognized value (including a typo, or empty) falls back to
    /// [`Self::Inherit`] rather than a parse error: this crate's config
    /// layer never fails a load over one bad setting (`banto_io::config`'s
    /// module doc).
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "select" => Self::Select,
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            _ => Self::Inherit,
        }
    }
}

impl<'de> Deserialize<'de> for WorkerAgentSetting {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::parse(&raw))
    }
}

/// Brigade formation settings (emporium mode only).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BrigadeConfig {
    /// How many fresh Workers to auto-spawn when a brigade is formed.
    /// Clamped to 1..=8 wherever it's consumed — a raw, unclamped value here
    /// lets a config round-trip losslessly even if it's out of range.
    pub workers: u32,
    /// `--model` passed to an auto-spawned Claude Worker's `claude`
    /// invocation. An empty string is the escape hatch: no `--model` flag is
    /// passed, so the Worker inherits the operator's default model. Not
    /// validated here — an invalid model name is `claude`'s problem,
    /// surfaced in the Worker's own pane.
    ///
    /// Named without a product suffix because it predates
    /// [`Self::worker_model_codex`] and every config.toml that already sets
    /// it means exactly this — a Claude Worker's model — so it keeps that
    /// name and that meaning unconditionally rather than being renamed or
    /// repurposed into a product-agnostic default. See
    /// [`Self::worker_model_for`] for how the two fields combine once a
    /// brigade can have either product as Worker.
    pub worker_model: String,
    /// `--model` an auto-spawned Codex Worker launches with — the Codex
    /// sibling of [`Self::worker_model`], same empty-string escape hatch,
    /// independent default (Codex and Claude Code do not share a model
    /// namespace, so one shared field could never default sensibly for
    /// both at once).
    pub worker_model_codex: String,
    /// See [`WorkerAgentSetting`].
    pub worker_agent: WorkerAgentSetting,
    /// See [`RelayMode`].
    pub relay: RelayMode,
    /// Role briefing appended to a Director's system prompt at launch
    /// (`claude --append-system-prompt`).
    ///
    /// Without one, a brigade exists only in banto's data model and UI: the
    /// operator sees a Director pane beside its Workers, while the Director
    /// itself is handed three tool names and no notion that it leads a cell
    /// — and, since the relay only wakes a member that *has* mail, a cell
    /// whose Director never sends the first message stays inert forever.
    /// This is the text that closes that gap, so it is deliberately a
    /// setting and not a constant: it states banto's delegation policy, and
    /// that is the operator's call.
    ///
    /// `{brigade}` (id), `{token}` (this member), and `{peers}` (the
    /// addressable peers, comma-joined) are substituted. An empty string
    /// passes no flag at all.
    pub director_prompt: String,
    /// Role briefing appended to a Worker's system prompt at launch. Same
    /// substitutions and the same empty-string escape hatch as
    /// [`Self::director_prompt`]; deliberately states facts (who you are,
    /// how the mail works) rather than a work policy, which is the
    /// Director's to give.
    pub worker_prompt: String,
}

/// See [`BrigadeConfig::director_prompt`]. Written to delegate by default:
/// the operator's expectation on finding themselves in a brigade is that
/// the Workers get used without having to be pointed at, and the conditions
/// worth delegating under are named so that serial diagnostic work — where
/// a handoff costs more context than it saves — stays home.
const DEFAULT_DIRECTOR_PROMPT: &str = "\
You are the Director of banto brigade {brigade}. Your Workers: {peers}. \
They are live agent sessions in this same working directory, \
reachable through banto's MCP tools.

Use them. When a task splits into parts that can proceed independently — a \
broad search, an audit across many files, an independent second opinion, a \
long mechanical edit — hand it to a Worker with send_to_peer instead of \
working through it serially, and tell the operator in one line that you \
did. Keep work that is genuinely sequential, or that hinges on context only \
you hold, yourself.

Workers cannot see this conversation. Every instruction must carry its own \
context and say what you want back. Set `to` to address one Worker; omit it \
to broadcast. Call check_messages when banto nudges you, and at natural \
checkpoints while waiting on a Worker.";

/// See [`BrigadeConfig::worker_prompt`].
const DEFAULT_WORKER_PROMPT: &str = "\
You are {token}, a Worker in banto brigade {brigade}, working under its \
Director in this same working directory. banto relays between you: the \
Director's instructions arrive through check_messages (banto nudges you \
when something is waiting), and send_to_peer is how you report back — \
findings, results, questions. Nobody reads your pane's transcript, so put \
what matters in the message you send, not just in your own scrollback.";

impl Default for BrigadeConfig {
    fn default() -> Self {
        Self {
            workers: 1,
            worker_model: "sonnet".to_string(),
            worker_model_codex: "gpt-5.6-terra".to_string(),
            worker_agent: WorkerAgentSetting::default(),
            relay: RelayMode::Auto,
            director_prompt: DEFAULT_DIRECTOR_PROMPT.to_string(),
            worker_prompt: DEFAULT_WORKER_PROMPT.to_string(),
        }
    }
}

impl BrigadeConfig {
    /// [`Self::workers`] clamped to a sane 1..=8 range for actual use.
    pub fn worker_count(&self) -> usize {
        self.workers.clamp(1, 8) as usize
    }

    /// The briefing template for `role`, or `None` when it is empty (the
    /// "launch this member with no briefing at all" escape hatch).
    pub fn prompt_for(&self, role: BrigadeRole) -> Option<&str> {
        let template = match role {
            BrigadeRole::Director => &self.director_prompt,
            BrigadeRole::Worker => &self.worker_prompt,
        };
        (!template.is_empty()).then_some(template.as_str())
    }

    /// The `--model` value an auto-spawned Worker running `agent` should
    /// launch with, or `None` for "pass no `--model` flag at all" (either
    /// product's own empty-string escape hatch). Not called anywhere yet —
    /// `crate::engine`'s `spawn_worker` still reads [`Self::worker_model`]
    /// directly, unconditionally, until the Worker-agent-selection feature
    /// lands there; this exists so that wiring has one obvious, already-
    /// tested place for the per-product choice instead of reinventing it
    /// inline.
    pub fn worker_model_for(&self, agent: AgentKind) -> Option<&str> {
        let raw = match agent {
            AgentKind::ClaudeCode => &self.worker_model,
            AgentKind::Codex => &self.worker_model_codex,
        };
        (!raw.is_empty()).then_some(raw.as_str())
    }
}

/// Per-agent binary overrides. `None` for a given field means "look it up
/// on `$PATH`" — today's behavior, and still the default. Codex is not
/// reliably on `PATH` in practice (observed: present in two install
/// locations, absent from `PATH` until the shell that installed it was
/// reopened), which is the whole reason this exists; Claude gets the same
/// treatment for symmetry rather than a special case.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct AgentBinaries {
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
}

/// Emporium keybinding settings. Just the tmux-style prefix chord this
/// round — full user-remappable keymaps are out of scope (a scoped decision,
/// not an oversight: see `crate::engine`'s `PrefixKey`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct KeysConfig {
    /// The prefix chord for pane operations (tmux-style: press it, then a
    /// second key — see `crate::engine::PrefixAction` for what each one
    /// does). `"C-<char>"` for a Control chord (e.g. the default `"C-b"`),
    /// or a bare single character for an unmodified key. Parsed leniently in
    /// the `banto` bin crate, not here: `KeyCode`/`Modifiers` parsing from a
    /// raw chord string is `crate::engine::PrefixKey`'s job — this field is
    /// just the raw string, validated no further than "is it a string".
    pub prefix: String,
}

impl Default for KeysConfig {
    fn default() -> Self {
        Self {
            prefix: "C-b".to_string(),
        }
    }
}

/// Top-level banto configuration. Every field has a default and unknown keys
/// are ignored, so any subset of `config.toml` is valid.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub opener: OpenerMode,
    pub activity: ActivityConfig,
    pub brigade: BrigadeConfig,
    pub keys: KeysConfig,
    pub agent_binaries: AgentBinaries,
    /// Which agent products banto discovers sessions for: `"all"` (also
    /// what an absent or empty string means, and the default), or a
    /// comma-separated list of product names (`claude`, `codex`). Kept as
    /// the raw string here and resolved by [`resolve_agents`] rather than
    /// parsed into a set at deserialize time, so a name this build doesn't
    /// recognize degrades leniently (see that function's doc) instead of
    /// failing the whole document the way `OpenerMode`'s strict enum would.
    pub agents: String,
    /// Overrides the provider's default `~/.claude` location (read-only!).
    pub claude_home: Option<PathBuf>,
    /// Overrides the provider's default `~/.codex` location (read-only!).
    pub codex_home: Option<PathBuf>,
    /// Overrides `banto_io::config::default_db_path`.
    pub db_path: Option<PathBuf>,
}

/// The result of resolving [`Config::agents`]: the working set, plus enough
/// of what [`resolve_agents`] discarded along the way for a caller to tell
/// the operator about it (see that function's doc for why dropped names are
/// tracked at all rather than simply vanishing).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedAgents {
    /// The agent products banto should actually discover sessions for.
    pub enabled: BTreeSet<AgentKind>,
    /// Names from a non-empty setting that matched no known product, in the
    /// order they first appeared, each listed once even if repeated. Empty
    /// for `""`/`"all"`, or when every name in the list was recognized.
    pub ignored: Vec<String>,
    /// Whether every name in a non-empty setting went unrecognized, so
    /// `enabled` fell back to [`AgentKind::ALL`] rather than reflecting
    /// anything the operator actually wrote — worse than [`Self::ignored`]
    /// being merely non-empty: there, the setting still did *something*;
    /// here, it silently did nothing at all.
    pub fell_back_to_all: bool,
}

/// Resolve [`Config::agents`] into the set of agent products banto should
/// discover sessions for. Empty (including an absent field, which
/// deserializes to the empty string) or `"all"` (case-insensitive) resolves
/// to [`AgentKind::ALL`] — "all" means every product this build supports,
/// not a wildcard for products that don't exist yet. Otherwise each
/// comma-separated entry is looked up independently, matched
/// case-insensitively with surrounding whitespace trimmed.
///
/// An unrecognized name is dropped, not rejected: this crate's config layer
/// is lenient by design (`banto_io::config`'s module doc — a broken setting
/// must never prevent startup), and every other lenient fallback in this
/// file decays toward *less filtering*, never toward *less discovery*
/// (`RelayMode`'s unknown value falls back to the working default, a
/// malformed file falls back to the full default `Config`). So if every
/// name in a non-empty setting goes unrecognized — a typo, or a product this
/// build has never heard of — [`ResolvedAgents::enabled`] is
/// [`AgentKind::ALL`] too, the same as leaving `agents` unset, rather than
/// an empty set that would silently discover nothing with no visible cause.
///
/// Dropping a name silently would still leave the operator with no way to
/// find a typo short of reading this function's source, so every
/// unrecognized name is recorded in [`ResolvedAgents::ignored`] — this
/// function only collects the fact; deciding whether and how to tell the
/// operator (a status line, today) is `banto`'s job, not this crate's (see
/// this module's doc: no I/O, no UI, here).
pub fn resolve_agents(raw: &str) -> ResolvedAgents {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
        return ResolvedAgents {
            enabled: AgentKind::ALL.into_iter().collect(),
            ignored: Vec::new(),
            fell_back_to_all: false,
        };
    }
    let mut ignored = Vec::new();
    let recognized: BTreeSet<AgentKind> = trimmed
        .split(',')
        .filter_map(|name| {
            let name = name.trim();
            match name.to_ascii_lowercase().as_str() {
                "claude" => Some(AgentKind::ClaudeCode),
                "codex" => Some(AgentKind::Codex),
                _ => {
                    if !ignored.iter().any(|seen: &String| seen == name) {
                        ignored.push(name.to_string());
                    }
                    None
                }
            }
        })
        .collect();
    if recognized.is_empty() {
        ResolvedAgents {
            enabled: AgentKind::ALL.into_iter().collect(),
            ignored,
            fell_back_to_all: true,
        }
    } else {
        ResolvedAgents {
            enabled: recognized,
            ignored,
            fell_back_to_all: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deserialize `text` as a [`Config`], the same way `banto_io::config`'s
    /// real file loader does — but without a filesystem, since these tests
    /// are only about the types' `Deserialize` shape (that's `banto_io`'s
    /// job to test end to end, over a real temp file).
    fn parse(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn defaults_have_documented_values() {
        let config = Config::default();
        assert_eq!(config.opener, OpenerMode::InPlace);
        assert_eq!(config.activity.today_hours, 24);
        assert_eq!(config.activity.week_days, 7);
        assert_eq!(config.brigade.workers, 1);
        assert_eq!(config.brigade.worker_model, "sonnet");
        assert_eq!(config.brigade.worker_model_codex, "gpt-5.6-terra");
        assert_eq!(config.brigade.worker_agent, WorkerAgentSetting::Inherit);
        assert_eq!(config.brigade.relay, RelayMode::Auto);
        assert_eq!(config.keys.prefix, "C-b");
        assert_eq!(config.agents, "");
        assert_eq!(config.claude_home, None);
        assert_eq!(config.codex_home, None);
        assert_eq!(config.db_path, None);
    }

    #[test]
    fn keys_prefix_parses_from_toml() {
        let config = parse("[keys]\nprefix = \"C-a\"\n");
        assert_eq!(config.keys.prefix, "C-a");
    }

    #[test]
    fn keys_section_missing_yields_the_default_prefix() {
        let config = parse("opener = \"psmux\"\n");
        assert_eq!(config.keys.prefix, "C-b");
    }

    #[test]
    fn partial_brigade_section_fills_remaining_defaults() {
        let config = parse("[brigade]\nworkers = 3\n");
        assert_eq!(config.brigade.workers, 3);
        assert_eq!(config.brigade.worker_count(), 3);
        assert_eq!(config.brigade.worker_model, "sonnet");
        assert_eq!(config.brigade.worker_model_codex, "gpt-5.6-terra");
        assert_eq!(config.brigade.worker_agent, WorkerAgentSetting::Inherit);
        assert_eq!(config.brigade.relay, RelayMode::Auto);
    }

    #[test]
    fn brigade_worker_count_clamps_to_one_through_eight() {
        fn with_workers(workers: u32) -> BrigadeConfig {
            BrigadeConfig {
                workers,
                ..Default::default()
            }
        }
        assert_eq!(with_workers(0).worker_count(), 1);
        assert_eq!(with_workers(1).worker_count(), 1);
        assert_eq!(with_workers(8).worker_count(), 8);
        assert_eq!(with_workers(20).worker_count(), 8);
    }

    #[test]
    fn brigade_worker_model_and_relay_parse() {
        let config = parse("[brigade]\nworker_model = \"opus\"\nrelay = \"manual\"\n");
        assert_eq!(config.brigade.worker_model, "opus");
        assert_eq!(config.brigade.relay, RelayMode::Manual);
    }

    #[test]
    fn brigade_worker_model_empty_string_is_the_inherit_default_escape_hatch() {
        let config = parse("[brigade]\nworker_model = \"\"\n");
        assert_eq!(config.brigade.worker_model, "");
    }

    #[test]
    fn an_existing_config_toml_setting_only_worker_model_is_unaffected_by_the_codex_split() {
        // A file written before `worker_model_codex`/`worker_agent` existed
        // must keep meaning exactly what it always meant: this Worker's
        // model, full stop, with the Codex field and the new-setting field
        // both silently taking their own defaults.
        let config = parse("[brigade]\nworker_model = \"sonnet\"\n");
        assert_eq!(config.brigade.worker_model, "sonnet");
        assert_eq!(config.brigade.worker_model_codex, "gpt-5.6-terra");
        assert_eq!(config.brigade.worker_agent, WorkerAgentSetting::Inherit);
    }

    #[test]
    fn worker_model_codex_defaults_independently_of_worker_model() {
        let config = parse("[brigade]\nworker_model = \"opus\"\n");
        assert_eq!(config.brigade.worker_model, "opus");
        assert_eq!(
            config.brigade.worker_model_codex, "gpt-5.6-terra",
            "an override for the Claude field must not leak into the Codex one"
        );
    }

    #[test]
    fn brigade_worker_model_codex_parses_and_has_its_own_empty_escape_hatch() {
        let overridden = parse("[brigade]\nworker_model_codex = \"gpt-5-mini\"\n");
        assert_eq!(overridden.brigade.worker_model_codex, "gpt-5-mini");

        let cleared = parse("[brigade]\nworker_model_codex = \"\"\n");
        assert_eq!(cleared.brigade.worker_model_codex, "");
    }

    #[test]
    fn worker_model_for_reads_the_matching_products_field_and_honors_both_escape_hatches() {
        let brigade = BrigadeConfig {
            worker_model: "opus".to_string(),
            worker_model_codex: String::new(),
            ..Default::default()
        };
        assert_eq!(
            brigade.worker_model_for(AgentKind::ClaudeCode),
            Some("opus")
        );
        assert_eq!(
            brigade.worker_model_for(AgentKind::Codex),
            None,
            "an empty worker_model_codex means no --model flag, same escape \
             hatch as worker_model"
        );
    }

    #[test]
    fn worker_model_for_uses_each_products_own_default_when_unset() {
        let brigade = BrigadeConfig::default();
        assert_eq!(
            brigade.worker_model_for(AgentKind::ClaudeCode),
            Some("sonnet")
        );
        assert_eq!(
            brigade.worker_model_for(AgentKind::Codex),
            Some("gpt-5.6-terra")
        );
    }

    // -- WorkerAgentSetting ----------------------------------------------

    #[test]
    fn worker_agent_defaults_to_inherit() {
        assert_eq!(WorkerAgentSetting::default(), WorkerAgentSetting::Inherit);
        assert_eq!(WorkerAgentSetting::parse(""), WorkerAgentSetting::Inherit);
    }

    #[test]
    fn worker_agent_every_known_value_parses() {
        for (text, expected) in [
            ("inherit", WorkerAgentSetting::Inherit),
            ("select", WorkerAgentSetting::Select),
            ("claude", WorkerAgentSetting::Claude),
            ("codex", WorkerAgentSetting::Codex),
        ] {
            assert_eq!(WorkerAgentSetting::parse(text), expected);
            let config = parse(&format!("[brigade]\nworker_agent = \"{text}\"\n"));
            assert_eq!(config.brigade.worker_agent, expected);
        }
    }

    #[test]
    fn worker_agent_is_case_insensitive_and_trims_whitespace() {
        for text in ["Codex", "CODEX", "  codex  "] {
            assert_eq!(WorkerAgentSetting::parse(text), WorkerAgentSetting::Codex);
        }
    }

    #[test]
    fn worker_agent_unknown_value_falls_back_to_inherit() {
        let config = parse("[brigade]\nworker_agent = \"made-up-product\"\n");
        assert_eq!(config.brigade.worker_agent, WorkerAgentSetting::Inherit);
    }

    #[test]
    fn brigade_briefings_default_to_something_that_names_the_tools_and_substitutes() {
        // Asserted structurally, not word for word: the prose is meant to be
        // edited (it is banto's delegation policy, see `director_prompt`),
        // and a test that pins its exact wording would just be a second copy
        // to keep in sync.
        let config = parse("");
        let director = config.brigade.prompt_for(BrigadeRole::Director).unwrap();
        assert!(director.contains("{brigade}") && director.contains("{peers}"));
        assert!(director.contains("send_to_peer") && director.contains("check_messages"));

        let worker = config.brigade.prompt_for(BrigadeRole::Worker).unwrap();
        assert!(worker.contains("{brigade}") && worker.contains("{token}"));
        assert!(worker.contains("send_to_peer") && worker.contains("check_messages"));
    }

    #[test]
    fn brigade_briefings_are_overridable_and_an_empty_one_launches_with_no_flag() {
        let config = parse("[brigade]\ndirector_prompt = \"lead {peers}\"\nworker_prompt = \"\"\n");
        assert_eq!(
            config.brigade.prompt_for(BrigadeRole::Director),
            Some("lead {peers}")
        );
        assert_eq!(config.brigade.prompt_for(BrigadeRole::Worker), None);
    }

    #[test]
    fn brigade_relay_unknown_value_falls_back_to_auto() {
        let config = parse("[brigade]\nrelay = \"sometimes\"\n");
        assert_eq!(config.brigade.relay, RelayMode::Auto);
    }

    #[test]
    fn partial_toml_fills_remaining_defaults() {
        let config = parse("opener = \"psmux\"\n");
        assert_eq!(config.opener, OpenerMode::Psmux);
        assert_eq!(config.activity, ActivityConfig::default());
        assert_eq!(config.db_path, None);
    }

    #[test]
    fn partial_activity_section_fills_remaining_defaults() {
        let config = parse("[activity]\ntoday_hours = 12\n");
        assert_eq!(config.activity.today_hours, 12);
        assert_eq!(config.activity.week_days, 7);
    }

    #[test]
    fn all_opener_values_parse() {
        for (text, expected) in [
            ("in-place", OpenerMode::InPlace),
            ("auto", OpenerMode::Auto),
            ("psmux", OpenerMode::Psmux),
            ("windows-terminal", OpenerMode::WindowsTerminal),
        ] {
            let config = parse(&format!("opener = \"{text}\"\n"));
            assert_eq!(config.opener, expected);
        }
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config = parse("opener = \"psmux\"\nfuture_option = true\n[some_new_section]\nx = 1\n");
        assert_eq!(config.opener, OpenerMode::Psmux);
    }

    #[test]
    fn agent_binaries_default_to_none() {
        let config = Config::default();
        assert_eq!(config.agent_binaries.claude, None);
        assert_eq!(config.agent_binaries.codex, None);
    }

    #[test]
    fn agent_binaries_parse_independently() {
        let config = parse("[agent_binaries]\ncodex = \"C:/tools/codex.exe\"\n");
        assert_eq!(config.agent_binaries.claude, None);
        assert_eq!(
            config.agent_binaries.codex,
            Some(PathBuf::from("C:/tools/codex.exe"))
        );
    }

    #[test]
    fn path_overrides_parse() {
        let config = parse(
            "claude_home = \"C:/synthetic/claude-home\"\n\
             codex_home = \"C:/synthetic/codex-home\"\n\
             db_path = \"C:/synthetic/banto.db\"\n",
        );
        assert_eq!(
            config.claude_home,
            Some(PathBuf::from("C:/synthetic/claude-home"))
        );
        assert_eq!(
            config.codex_home,
            Some(PathBuf::from("C:/synthetic/codex-home"))
        );
        assert_eq!(config.db_path, Some(PathBuf::from("C:/synthetic/banto.db")));
    }

    #[test]
    fn wrong_field_type_is_a_parse_error() {
        let result: Result<Config, _> = toml::from_str("opener = \"no-such-backend\"\n");
        assert!(result.is_err());
    }

    #[test]
    fn agents_setting_parses_as_a_plain_string() {
        let config = parse("agents = \"codex\"\n");
        assert_eq!(config.agents, "codex");
    }

    // -- resolve_agents ------------------------------------------------

    fn all_agents() -> BTreeSet<AgentKind> {
        AgentKind::ALL.into_iter().collect()
    }

    #[test]
    fn resolve_agents_empty_means_all() {
        let resolved = resolve_agents("");
        assert_eq!(resolved.enabled, all_agents());
        assert!(resolved.ignored.is_empty());
        assert!(!resolved.fell_back_to_all);
    }

    #[test]
    fn resolve_agents_all_is_case_insensitive_and_trims_whitespace() {
        for text in ["all", "ALL", "All", "  all  "] {
            let resolved = resolve_agents(text);
            assert_eq!(resolved.enabled, all_agents());
            assert!(resolved.ignored.is_empty());
            assert!(!resolved.fell_back_to_all);
        }
    }

    #[test]
    fn resolve_agents_single_name() {
        assert_eq!(
            resolve_agents("codex").enabled,
            BTreeSet::from([AgentKind::Codex])
        );
        assert_eq!(
            resolve_agents("claude").enabled,
            BTreeSet::from([AgentKind::ClaudeCode])
        );
    }

    #[test]
    fn resolve_agents_comma_separated_list_is_case_insensitive_and_trims_whitespace() {
        assert_eq!(
            resolve_agents(" Claude ,CODEX ").enabled,
            BTreeSet::from([AgentKind::ClaudeCode, AgentKind::Codex])
        );
    }

    #[test]
    fn resolve_agents_drops_an_unrecognized_name_alongside_a_recognized_one() {
        let resolved = resolve_agents("claude,made-up-product");
        assert_eq!(resolved.enabled, BTreeSet::from([AgentKind::ClaudeCode]));
        assert_eq!(resolved.ignored, vec!["made-up-product".to_string()]);
        assert!(
            !resolved.fell_back_to_all,
            "a partial drop still used a real name, not the fallback"
        );
    }

    #[test]
    fn resolve_agents_falls_back_to_all_when_nothing_is_recognized() {
        let single = resolve_agents("made-up-product");
        assert_eq!(single.enabled, all_agents());
        assert_eq!(single.ignored, vec!["made-up-product".to_string()]);
        assert!(single.fell_back_to_all);

        let list = resolve_agents("nonsense,also-nonsense");
        assert_eq!(list.enabled, all_agents());
        assert_eq!(
            list.ignored,
            vec!["nonsense".to_string(), "also-nonsense".to_string()]
        );
        assert!(list.fell_back_to_all);
    }

    #[test]
    fn resolve_agents_a_repeated_name_still_yields_one_entry() {
        assert_eq!(
            resolve_agents("codex,codex").enabled,
            BTreeSet::from([AgentKind::Codex])
        );
    }

    #[test]
    fn resolve_agents_a_repeated_unrecognized_name_is_listed_once() {
        let resolved = resolve_agents("claude,made-up,made-up");
        assert_eq!(resolved.ignored, vec!["made-up".to_string()]);
    }
}
