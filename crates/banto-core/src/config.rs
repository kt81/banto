//! banto configuration types.
//!
//! Every field has a default and unknown keys are ignored (`#[serde(default)]`
//! throughout), so any subset of a TOML document deserializes into a valid
//! [`Config`] — that leniency is the contract these types promise; loading
//! the actual `config.toml` file (locating it, reading it, turning a parse
//! failure into an error or a silent default) is `banto_io::config`'s job —
//! it needs filesystem access and the `dirs` crate for the default path,
//! both forbidden here (`docs/DISCIPLINE.md` §2).

use std::path::PathBuf;

use serde::Deserialize;

use crate::model::BrigadeRole;

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

/// Brigade formation settings (emporium mode only).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BrigadeConfig {
    /// How many fresh Workers to auto-spawn when a brigade is formed.
    /// Clamped to 1..=8 wherever it's consumed — a raw, unclamped value here
    /// lets a config round-trip losslessly even if it's out of range.
    pub workers: u32,
    /// `--model` passed to an auto-spawned Worker's `claude` invocation. An
    /// empty string is the escape hatch: no `--model` flag is passed, so the
    /// Worker inherits the operator's default model. Not validated here —
    /// an invalid model name is `claude`'s problem, surfaced in the Worker's
    /// own pane.
    pub worker_model: String,
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
They are live Claude Code sessions in this same working directory, \
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
    /// Overrides the provider's default `~/.claude` location (read-only!).
    pub claude_home: Option<PathBuf>,
    /// Overrides `banto_io::config::default_db_path`.
    pub db_path: Option<PathBuf>,
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
        assert_eq!(config.brigade.relay, RelayMode::Auto);
        assert_eq!(config.keys.prefix, "C-b");
        assert_eq!(config.claude_home, None);
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
            "claude_home = \"C:/synthetic/claude-home\"\ndb_path = \"C:/synthetic/banto.db\"\n",
        );
        assert_eq!(
            config.claude_home,
            Some(PathBuf::from("C:/synthetic/claude-home"))
        );
        assert_eq!(config.db_path, Some(PathBuf::from("C:/synthetic/banto.db")));
    }

    #[test]
    fn wrong_field_type_is_a_parse_error() {
        let result: Result<Config, _> = toml::from_str("opener = \"no-such-backend\"\n");
        assert!(result.is_err());
    }
}
