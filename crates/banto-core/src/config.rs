//! banto configuration types.
//!
//! Every field has a default and unknown keys are ignored (`#[serde(default)]`
//! throughout), so any subset of a TOML document deserializes into a valid
//! [`Config`]. A key that is there must be right, though: a value of the wrong
//! TOML type, or a string that is not one of the words its setting takes
//! ([`not_accepted`]), is a deserialization error, so that `toml` reports its
//! line and column and banto does not start on it. Free text (a model, an
//! effort, a prompt) has no fixed set and is taken as written. Loading the
//! actual `config.toml` file (locating it, reading it, turning a parse failure
//! into an error) is `banto_io::config`'s job — it needs filesystem access and
//! the `dirs` crate for the default path, both forbidden here
//! (`docs/DISCIPLINE.md` §2).

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::Deserialize;

use crate::model::{AgentKind, BrigadeRole};

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

/// `"auto"` or `"manual"`, trimmed and case-insensitive like the other word
/// settings.
impl<'de> Deserialize<'de> for RelayMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(RelayMode::Auto),
            "manual" => Ok(RelayMode::Manual),
            _ => Err(not_accepted("relay", &raw, "\"auto\" or \"manual\"")),
        }
    }
}

/// Which product an auto-spawned Worker runs as, relative to the Director
/// forming the brigade — `[brigade] worker_agent` in config.toml.
///
/// Read by [`crate::engine::resolve_worker_agent`] at both formation
/// ([`crate::engine::begin_brigade_formation`]) and single-Worker addition
/// ([`crate::engine::add_worker`]); [`Self::Select`] opens the same
/// `Modal::WorkerAgentPicker` in both places, resolved by
/// [`crate::engine::confirm_worker_agent_modal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkerAgentSetting {
    /// Same product as the Director. The default: an operator who never
    /// heard of this setting keeps getting exactly what banto has always
    /// spawned for a Claude Director, and a Codex Director's Workers follow
    /// it symmetrically rather than silently defaulting to Claude anyway.
    #[default]
    Inherit,
    /// Ask whenever Workers are about to be created — forming a brigade and
    /// adding a Worker (a picker: the agent, the model and the effort, the same
    /// axis the new-session modal already offers for a plain session — decided
    /// in the UI layer, not here). It opens on the agent confirmed last time
    /// with that agent's defaults, and what is confirmed is what those Workers
    /// start with.
    Select,
    /// Always Claude Code, regardless of the Director's own product.
    Claude,
    /// Always Codex, regardless of the Director's own product.
    Codex,
}

impl WorkerAgentSetting {
    /// Parses `[brigade] worker_agent`'s raw string. Case-insensitive and
    /// whitespace-trimmed, like the neighboring `agents` setting, since both
    /// name the same product vocabulary an operator might type inconsistently.
    /// `None` for anything else, an empty string included: it is not one of the
    /// four words, and there is no "no setting" spelling (omit the key).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "inherit" => Some(Self::Inherit),
            "select" => Some(Self::Select),
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for WorkerAgentSetting {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            not_accepted(
                "worker_agent",
                &raw,
                "\"inherit\", \"claude\", \"codex\" or \"select\"",
            )
        })
    }
}

/// Which product an auto-spawned Goinkyo runs as — `[brigade] goinkyo_agent` in
/// config.toml. The built-in is [`Self::Inherit`], so a Codex Director's
/// Goinkyo is Codex; `claude` keeps the product a Goinkyo had before the key
/// existed, whatever the Director runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GoinkyoAgentSetting {
    /// The Director's own product: a Codex Director consults a Codex Goinkyo.
    /// The built-in.
    #[default]
    Inherit,
    /// Claude Code, whatever the Director runs.
    Claude,
    /// Codex.
    Codex,
}

impl GoinkyoAgentSetting {
    /// Parses `[brigade] goinkyo_agent`'s raw string like
    /// [`WorkerAgentSetting::parse`]: case-insensitive and whitespace-trimmed.
    /// `None` for anything else, an empty string included: it is not one of the
    /// three words, and the way to take the built-in is to omit the key.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "inherit" => Some(Self::Inherit),
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }
}

/// The product a Goinkyo runs as, given the operator's setting and the product
/// of the Director that summons it. The one place `inherit` is resolved:
/// `consult_goinkyo` calls it to decide whether the consultation can start at
/// all and records the answer on the Goinkyo's member row, and the engine
/// calls it only for a row that has none (a row written before the product was
/// recorded), so the two cannot disagree after the config is edited.
pub fn resolve_goinkyo_agent(setting: GoinkyoAgentSetting, director: AgentKind) -> AgentKind {
    match setting {
        GoinkyoAgentSetting::Inherit => director,
        GoinkyoAgentSetting::Claude => AgentKind::ClaudeCode,
        GoinkyoAgentSetting::Codex => AgentKind::Codex,
    }
}

/// Why a Goinkyo of a given product cannot be started, as
/// [`goinkyo_refusal`] reports it. Each variant is a condition the operator can
/// change; none is a judgement about the consultation itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoinkyoRefusal {
    /// The `agents` setting leaves the product out, so banto neither lists nor
    /// opens its sessions.
    NotEnabled(AgentKind),
    /// The product's home directory did not resolve, so there is nothing to
    /// launch it against.
    HomeUnresolved(AgentKind),
    /// A Codex Goinkyo would start unbriefed: banto's own path has a space in it,
    /// so the hook command cannot carry it and Codex is never given the hook.
    CodexHookNotLaunchable,
    /// A Codex Goinkyo would start unbriefed: Codex has not been asked to trust
    /// banto's `SessionStart` hook, and an untrusted hook fails silently.
    CodexHookNotPrimed,
}

/// The two facts about banto's `SessionStart` hook that decide whether a Codex
/// member is briefed. Named fields rather than two bare bools, which a caller
/// could swap without the compiler noticing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexHookFacts {
    /// The hook command can carry banto's executable path (no space in it).
    pub launchable: bool,
    /// Codex has been asked to trust the hook (`Primed`).
    pub primed: bool,
}

/// The first condition that stops a Goinkyo of `product` from working, in the
/// order an operator would fix them: the product enabled, its home known, then
/// (Codex only) the hook launchable and the hook trusted. A hook that cannot be
/// launched outranks the trust question, as it does where a brigade is formed
/// (`update_codex_trust_checked`): a trust record for a hook that never runs
/// settles nothing. `None` when nothing does.
///
/// Directory trust is not among them: Codex writes no trust record for a
/// directory outside a git repository, so an absent one does not mean it will
/// ask (`banto_io::directory_trust`'s module doc).
pub fn goinkyo_refusal(
    product: AgentKind,
    enabled: &BTreeSet<AgentKind>,
    home_resolved: bool,
    codex_hook: CodexHookFacts,
) -> Option<GoinkyoRefusal> {
    if !enabled.contains(&product) {
        return Some(GoinkyoRefusal::NotEnabled(product));
    }
    if !home_resolved {
        return Some(GoinkyoRefusal::HomeUnresolved(product));
    }
    if product != AgentKind::Codex {
        return None;
    }
    if !codex_hook.launchable {
        return Some(GoinkyoRefusal::CodexHookNotLaunchable);
    }
    (!codex_hook.primed).then_some(GoinkyoRefusal::CodexHookNotPrimed)
}

impl<'de> Deserialize<'de> for GoinkyoAgentSetting {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            not_accepted(
                "goinkyo_agent",
                &raw,
                "\"inherit\", \"claude\" or \"codex\"",
            )
        })
    }
}

/// `[brigade.claude]`: the settings that only mean something to Claude Code.
/// Every one is optional: an absent key defers to the legacy flat `[brigade]`
/// key of the same name (see [`BrigadeConfig`]) and then to the built-in. Values
/// that go to the agent are passed on as written and not validated: banto
/// keeps no list of the harness's models, efforts or modes, and the agent says
/// what it thinks of a value it does not know.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct ClaudeBrigadeConfig {
    /// `--model` a Claude Worker launches with **when its session is first
    /// created** — never on a resume: a flag-less `claude --resume` restores
    /// the transcript's last model, including one the operator changed inside
    /// the session, and a `--model` here would put it back (read in Claude
    /// Code's docs for 2.1.323, not measured by running it). An empty string
    /// is the escape hatch: no `--model` flag at all, so a new Worker starts
    /// on the operator's own default. Not validated here — an unknown model
    /// fails at the first API call, in the Worker's own pane.
    ///
    /// The built-in is the family word `sonnet`, never a version: Claude
    /// resolves the alias itself, so it cannot go stale in this file.
    ///
    /// [`BrigadeConfig::worker_model_for`] picks between this and
    /// [`CodexBrigadeConfig::worker_model`] by the Worker's product.
    pub worker_model: Option<String>,
    /// `--effort <level>` a Claude Worker launches with when its session is
    /// first created (`low`/`medium`/`high`/`xhigh`/`max`), built-in `high`;
    /// empty passes no flag. Not validated here: Claude warns about an
    /// invalid level and ignores it. Never passed on a resume — the effort
    /// then comes from the operator's own Claude settings, which Claude saves
    /// per model (read in its docs, not measured by running it).
    pub worker_effort: Option<String>,
    /// `--permission-mode <mode>` an auto-spawned Worker launches with,
    /// default `"auto"` — same flag, same accepted values, and the same
    /// unattended-operator rationale as [`Self::goinkyo_permission_mode`].
    /// Same empty-string escape hatch as [`Self::worker_model`].
    ///
    /// Unlike the Goinkyo, a Worker carries no accompanying tool denylist
    /// alongside this: editing the working tree is what a Worker is for, so
    /// nothing here pairs with it the way [`Self::goinkyo_disallowed_tools`]
    /// pairs with the Goinkyo's.
    pub worker_permission_mode: Option<String>,
    /// `--model` a Goinkyo is created with, built-in `"opus"` (a family
    /// word, no version). Only when its session is first created, never on a
    /// resume — [`Self::worker_model`]'s doc says why. Same empty-string escape
    /// hatch. A Claude Goinkyo's alone: it is a Claude word. A Codex Goinkyo
    /// (`goinkyo_agent = "codex"`) reads [`CodexBrigadeConfig::goinkyo_model`].
    ///
    /// `fable` is an opt-in, not the built-in: a Pro or standard Team seat pays
    /// for Fable with usage credits and an interactive session asks consent
    /// first, a prompt that waits with no expiry in a pane nobody is watching
    /// (`docs/REQUIREMENTS.md`, "Decision (2026-11-23)").
    pub goinkyo_model: Option<String>,
    /// `--effort <level>` a Goinkyo is created with, built-in `"xhigh"`, on
    /// creation only like the model above. `max` is an opt-in: it cannot be
    /// saved to Claude's settings, so a resumed Goinkyo never has it again,
    /// and a Goinkyo that ran on it was a quarter of this project's measured
    /// usage (same decision entry). Claude's own reasoning-effort flag (`claude --help`,
    /// accepts `low`/`medium`/`high`/`xhigh`/`max`): only a Claude Goinkyo is
    /// given it, and a Codex one reads [`CodexBrigadeConfig::goinkyo_effort`],
    /// which is a different flag with different words. Same empty-string escape
    /// hatch as the model above.
    pub goinkyo_effort: Option<String>,
    /// `--permission-mode <mode>` an auto-spawned Goinkyo launches with,
    /// default `"auto"` (`claude --help`, accepts `manual`/`acceptEdits`/
    /// `plan`/`auto`/`dontAsk`/`bypassPermissions`). Same empty-string
    /// escape hatch as the model/effort above.
    ///
    /// `auto` and not `manual` (Claude's own default) because nobody is at
    /// the Goinkyo's keyboard to answer a permission prompt — `manual`
    /// reliably stops there, and so does `plan`, whose own design still
    /// exits through a human approval at the end. Not a guarantee of
    /// unattended operation either, though: Claude Code falls back to its
    /// ordinary confirmation flow after repeated denials even under `auto`,
    /// and that flow stops the same way `manual` does. This only removes
    /// one specific stop, observed live 2026-08-05 — a Goinkyo that had
    /// just started on its own then sat
    /// waiting for an operator who wasn't watching to approve a tool call.
    pub goinkyo_permission_mode: Option<String>,
    /// `--disallowedTools <tools>` an auto-spawned Goinkyo launches with,
    /// default `"Edit,Write,NotebookEdit"` (`claude --help`: comma- or
    /// space-separated tool names to deny). Same empty-string escape hatch
    /// as the model/effort/permission-mode above, and — unlike `effort`,
    /// like `permission_mode` — passed on a resume too: which tools a
    /// Goinkyo may use is a property of the *role*, not of whether this
    /// particular launch happens to be fresh or resumed.
    ///
    /// Not containment: Bash stays available (this is a tool-name denylist,
    /// not a sandbox), so it is no barrier against anything actually
    /// adversarial. It exists because `goinkyo_permission_mode`'s own
    /// `"auto"` default changed what "unattended" means here — under
    /// `"manual"` an edit stopped for approval nobody was there to give;
    /// under `"auto"` it doesn't, so an unattended Goinkyo could now write
    /// to the same working tree the Director is using without anyone
    /// having agreed to that. This is a deterrent against that drift, not a
    /// defense against a hostile session. The cost worth stating plainly:
    /// it also blocks whatever a scratchpad or memory write would use
    /// (`Edit`/`Write`/`NotebookEdit`) — consistent with a role meant to be
    /// called holding nothing of its own, but a real cost, not a free one.
    pub goinkyo_disallowed_tools: Option<String>,
}

/// `[brigade.codex]`: the settings that only mean something to Codex. Resolved
/// like [`ClaudeBrigadeConfig`]'s, except that its two Goinkyo keys have no
/// legacy flat spelling, so their built-in is the empty string.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct CodexBrigadeConfig {
    /// The model a Codex Worker launches with when its session is first
    /// created — the Codex sibling of [`ClaudeBrigadeConfig::worker_model`], same
    /// empty-string escape hatch, independent default (the products share no
    /// model namespace).
    ///
    /// Either a slug or a family word. Codex has no alias for `-m`: the string
    /// must be a catalog slug, and an unknown one is sent to the API as it
    /// stands. So a family word (the built-in `terra`) is resolved to the
    /// newest slug of that family from the installed binary's own catalog at
    /// the moment the session is created (`crate::codex_model`); a string
    /// holding a digit that the catalog does not know is passed untouched.
    pub worker_model: Option<String>,
    /// `-c model_reasoning_effort="<level>"` a Codex Worker launches with when
    /// its session is first created, built-in `high`; empty passes nothing.
    /// Never on a resume: a `-c model_reasoning_effort` on a resume switches
    /// Codex to "override" mode and drops the thread's own model (read in
    /// Codex's source at tag `rust-v0.168.0`, not measured by running it).
    pub worker_effort: Option<String>,
    /// The model a Codex Goinkyo launches with when its session is first
    /// created. Built-in empty: no `-m`, so it starts on the operator's own Codex
    /// default, which is what a Goinkyo asked for in Codex has run on since
    /// `goinkyo_agent` existed. A slug or a family word, like
    /// [`Self::worker_model`], and held back for the catalog the same way
    /// (`crate::codex_model`). Creation only, never on a resume, for the reason
    /// [`Self::worker_effort`] gives. Not validated.
    pub goinkyo_model: Option<String>,
    /// `-c model_reasoning_effort="<level>"` a Codex Goinkyo launches with when
    /// its session is first created. Built-in empty: nothing is passed. Creation
    /// only, for the reason [`Self::worker_effort`] gives. Not validated.
    pub goinkyo_effort: Option<String>,
}

/// Brigade formation settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BrigadeConfig {
    /// How many fresh Workers to auto-spawn when a brigade is formed.
    /// Clamped to 1..=8 wherever it's consumed — a raw, unclamped value here
    /// lets a config round-trip losslessly even if it's out of range.
    pub workers: u32,
    /// Legacy spelling of `[brigade.claude] worker_model`, still read; the table's
    /// value wins when both are set. Holds the built-in when the file sets
    /// neither, which is why it is not an `Option`. What the value means is on
    /// [`ClaudeBrigadeConfig::worker_model`].
    pub worker_model: String,
    /// Legacy spelling of `[brigade.claude] worker_effort`, still read; the table's
    /// value wins when both are set. Holds the built-in when the file sets
    /// neither, which is why it is not an `Option`. What the value means is on
    /// [`ClaudeBrigadeConfig::worker_effort`].
    pub worker_effort: String,
    /// The window a Worker compacts at: `auto`, or a size in tokens from
    /// 100000 to 1000000. Built-in `250000`; an empty string passes nothing.
    /// Written as a TOML integer (`250000`) or as a string (`"auto"`,
    /// `"250000"`). A Claude Worker gets it as `--autocompact <value>`; a Codex
    /// Worker gets a token count as `-c model_auto_compact_token_limit=<n>` and
    /// nothing for `auto` ([`Self::worker_autocompact_for`]).
    ///
    /// Unlike the model and the effort, passed on *every* launch of a Worker, a
    /// resume included: it is given as a launch flag, and banto does not count
    /// on a resumed session remembering it. Never passed to a Director or a
    /// Goinkyo, or to a pane banto did not launch as a member: their
    /// compaction stays whatever the agent's own settings say.
    ///
    /// The clamp and the resume rule below come from upstream `main` as read on
    /// 2026-11-16 and the installed codex-cli 0.169.0's help and strings, not
    /// from running it. Codex uses the key only to lower its threshold: the key
    /// is clamped to 90% of the model's window (500k for gpt-6.1-terra, 272k
    /// for the gpt-6 and gpt-5.6 families, so 450k and 244.8k), and a value
    /// above the clamp changes nothing. A resumed thread keeps its saved model
    /// and effort unless the overrides name one, and this key names neither, so
    /// passing it on a resume is safe. One resumed thread was watched, on
    /// 2026-11-16: a Worker resumed with 250000, whose previous turn had ended
    /// at 288k of a 475k window, compacted before the first model call of its
    /// next turn. The default threshold there is 450k by the source, and the
    /// same thread's earlier compaction had come at 448k; no run without the
    /// override was made to compare.
    ///
    /// A value outside that set, or a TOML type that is neither an integer nor a
    /// string, is refused when the file is read. Kept as the text the operator
    /// wrote, as every other setting here is; [`Self::worker_autocompact`] is
    /// the one reader.
    #[serde(deserialize_with = "autocompact_setting")]
    pub worker_autocompact: String,
    /// Legacy spelling of `[brigade.codex] worker_model`, still read; the table's
    /// value wins when both are set. Holds the built-in when the file sets
    /// neither, which is why it is not an `Option`. What the value means is on
    /// [`CodexBrigadeConfig::worker_model`].
    pub worker_model_codex: String,
    /// Legacy spelling of `[brigade.codex] worker_effort`, still read; the table's
    /// value wins when both are set. Holds the built-in when the file sets
    /// neither, which is why it is not an `Option`. What the value means is on
    /// [`CodexBrigadeConfig::worker_effort`].
    pub worker_effort_codex: String,
    /// See [`WorkerAgentSetting`].
    pub worker_agent: WorkerAgentSetting,
    /// Legacy spelling of `[brigade.claude] worker_permission_mode`, still read; the table's
    /// value wins when both are set. Holds the built-in when the file sets
    /// neither, which is why it is not an `Option`. What the value means is on
    /// [`ClaudeBrigadeConfig::worker_permission_mode`].
    pub worker_permission_mode: String,
    /// See [`RelayMode`].
    pub relay: RelayMode,
    /// Role briefing appended to a Director's system prompt at launch
    /// (`claude --append-system-prompt`; a Codex Director gets it through the
    /// `SessionStart` hook instead, `crates/banto/src/hook.rs`).
    ///
    /// Without one, a brigade exists only in banto's data model and UI: the
    /// operator sees a Director pane beside its Workers, while the Director
    /// itself is handed banto's tool names and no notion that it leads a cell
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
    /// Role briefing appended to a Goinkyo's system prompt at launch. Same
    /// substitutions as [`Self::director_prompt`]/[`Self::worker_prompt`],
    /// plus `{request}` (the consultation request file's path — see
    /// `crate::briefing::render`), and the same empty-string escape hatch.
    pub goinkyo_prompt: String,
    /// See [`GoinkyoAgentSetting`]. A Codex Goinkyo takes its model
    /// and effort from `[brigade.codex]` ([`CodexBrigadeConfig::goinkyo_model`],
    /// [`CodexBrigadeConfig::goinkyo_effort`]), never from the Claude ones, and
    /// none of the Claude-only flags that keep a Goinkyo from editing
    /// (`[brigade.claude]`'s `goinkyo_permission_mode` and
    /// `goinkyo_disallowed_tools`); it is held read-only by Codex's own sandbox
    /// flags instead.
    pub goinkyo_agent: GoinkyoAgentSetting,
    /// Legacy spelling of `[brigade.claude] goinkyo_model`, still read; the table's
    /// value wins when both are set. Holds the built-in when the file sets
    /// neither, which is why it is not an `Option`. What the value means is on
    /// [`ClaudeBrigadeConfig::goinkyo_model`].
    pub goinkyo_model: String,
    /// Legacy spelling of `[brigade.claude] goinkyo_effort`, still read; the table's
    /// value wins when both are set. Holds the built-in when the file sets
    /// neither, which is why it is not an `Option`. What the value means is on
    /// [`ClaudeBrigadeConfig::goinkyo_effort`].
    pub goinkyo_effort: String,
    /// Legacy spelling of `[brigade.claude] goinkyo_permission_mode`, still read; the table's
    /// value wins when both are set. Holds the built-in when the file sets
    /// neither, which is why it is not an `Option`. What the value means is on
    /// [`ClaudeBrigadeConfig::goinkyo_permission_mode`].
    pub goinkyo_permission_mode: String,
    /// Legacy spelling of `[brigade.claude] goinkyo_disallowed_tools`, still read; the table's
    /// value wins when both are set. Holds the built-in when the file sets
    /// neither, which is why it is not an `Option`. What the value means is on
    /// [`ClaudeBrigadeConfig::goinkyo_disallowed_tools`].
    pub goinkyo_disallowed_tools: String,
    /// `[brigade.claude]`: where the Claude-only settings live now.
    #[serde(deserialize_with = "claude_table")]
    pub claude: ClaudeBrigadeConfig,
    /// `[brigade.codex]`: where the Codex-only settings live now.
    #[serde(deserialize_with = "codex_table")]
    pub codex: CodexBrigadeConfig,
    /// The legacy keys the file wrote, in [`LEGACY_BRIGADE_KEYS`]' order. Not
    /// read by serde: a value only says it was set when it differs from the
    /// built-in, and this crate has no TOML parser to ask the file, so
    /// `banto_io::config::load` fills it from the parsed table
    /// ([`legacy_brigade_keys_in`]). Empty for a config built in code. Only
    /// [`Self::legacy_key_notice`] reads it.
    #[serde(skip)]
    pub legacy_keys: Vec<&'static str>,
    /// How readily a Director consults the Goinkyo on its own initiative:
    /// `off`, `disputes`, `design` (built-in) or `decisions`, kept as the text
    /// the operator wrote ([`Self::goinkyo_consult`] is the one reader). Written
    /// to a Director as a paragraph after its `director_prompt`
    /// (`crate::briefing` in the `banto` crate), on both products. At every
    /// level the Director also consults when the operator asks.
    ///
    /// banto enforces none of them: each is an instruction in the briefing that
    /// moves the rate, `off` included, and `consult_goinkyo` works at `off`
    /// exactly as at any level. An empty string is the file's usual escape
    /// hatch, no paragraph. So is an empty `director_prompt`, which leaves
    /// nothing to append to: `off` cannot be expressed without a briefing. A
    /// value that is none of the levels, or not a string, is refused when the
    /// file is read. The UI reads the config once at start; a
    /// Claude Director carries the paragraph in its launch argv, so an edit needs
    /// banto restarted and the Director relaunched. `banto _hook` re-reads the
    /// file at every firing, so a Codex Director picks an edit up at its next
    /// `SessionStart` without a restart. A running Director never sees a change.
    #[serde(deserialize_with = "consult_setting")]
    pub goinkyo_consult: String,
    /// Append every brigade message, in full, to a plain-text file under banto's
    /// own data directory (`banto_io::brigade_log`) — for someone developing
    /// banto who wants to audit what the agents told each other. Off by default:
    /// the file holds real conversation content and banto never rotates or
    /// deletes it. The location is not configurable, so that it can never leave
    /// banto's own directory.
    pub message_log: bool,
    /// Whether a Director's `request_decision` is an attention event: on a new
    /// request the Director's list row is marked and the terminal bell rings,
    /// staged or not and wherever the operator is, and the brigade's quiet watch
    /// is spent (the quiet after the same stretch of work does not ring a second
    /// time). The tag that says the Director is waiting is drawn either way and
    /// stays until the operator answers in its pane. Default `true`; `false` keeps
    /// the tag and nothing else. Read once at start.
    pub request_attention: bool,
}

/// See [`BrigadeConfig::director_prompt`]. Written to delegate by default:
/// the operator's expectation on finding themselves in a brigade is that
/// the Workers get used without having to be pointed at, and the conditions
/// worth delegating under are named so that serial diagnostic work — where
/// a handoff costs more context than it saves — stays home.
///
/// The exception clause used to say "genuinely sequential" where this
/// comment says "diagnostic", and those are not the same set: nearly all
/// implementation is sequential in the small, almost none of it is
/// diagnostic. Measured against a Director that had just been cleared
/// (2026-08-08): every measurement, audit and review went to Workers and
/// every line of code stayed home, including a mechanical field-threading
/// edit — this paragraph's own named example — that was then botched by a
/// blind search-and-replace and cost several round trips of the one context
/// a brigade cannot refill. The operator noticed before the Director did.
/// So the clause now carries the comment's own test rather than a word that
/// happened to be broader than it, and the other side of the ledger is
/// priced: every paragraph below prices delegation, and nothing priced
/// keeping work, which is the whole skew a freshly-cleared Director reads
/// off this text and nothing else.
///
/// The sentence that prices a message exists because, at the close of one
/// session (2026-11-16), a Director broadcast "stop all work" to Workers that
/// had all reported. The relay types a nudge line into an idle member's pane,
/// which starts a turn, so each idle Worker was woken over its whole context
/// to read a message that asked for nothing. The worker briefing carries the
/// matching rule for replies.
const DEFAULT_DIRECTOR_PROMPT: &str = "\
You are the Director of banto brigade {brigade}. Your Workers: {peers}. \
They are live agent sessions in this same working directory, \
reachable through banto's MCP tools.

Use them. When a task splits into parts that can proceed independently — a \
broad search, an audit across many files, an independent second opinion, a \
long mechanical edit — hand it to a Worker with send_to_peer instead of \
working through it serially, and tell the operator in one line that you \
did. Keep work yourself only when the next step cannot be known until you \
have seen this one's result — diagnosis, live debugging — or when it hinges \
on context only you hold. If the errand can be written down completely \
before it starts, hand it over, however sequential its insides look. What \
you keep is paid out of the scarcer context: a Worker's is spent on one \
errand and thrown away, yours carries everything the operator has told you \
and every decision taken since. And when you keep something that matches a \
shape above, tell the operator that in one line too — the same as when you \
delegate.

Workers cannot see this conversation. Every instruction must carry its own \
context and say what you want back. Set `to` to address one Worker; omit it \
to broadcast. With banto's relay on, which is the default, a message wakes \
an idle recipient for a turn over its whole context. A wake waits while the \
member is busy; one nobody takes is shown as held. A wake is not guaranteed: \
the relay can be off, so \
a sent message is not a read one. Send a Worker only work, or a \
correction to work in flight, and a stop whenever its work may still be \
running (for a Claude Worker, interrupt_worker has banto refuse its next \
tool calls until it reads such a correction or stop, instead of leaving it \
for the end of the turn). Thanks, a stand-down and a closing note go to no Worker whose errand \
is finished and reported, not even when the operator ends the day. banto \
tells the operator when the brigade has gone quiet. Call check_messages when banto nudges you, and \
at natural checkpoints while waiting on a Worker. The Workers named above were \
the roster when you started, and it does not change afterwards — one \
added later will not appear in it. `brigade_status` has the live one. \
When you give a Worker a standing role, name_member gives it a display \
name, so that you, your Workers and the operator can tell them apart; \
its token stays its address.

When you say why something is the way it is — a cause, a mechanism, what \
broke — the ground for it is a Worker's report or something already \
settled in this conversation, not an impression you formed by reading \
just now. Neutralise the question before you hand it over: give the \
Worker what you want to know, not what you suspect, or your own guess \
comes back wearing citations. Read source yourself to see what a \
decision acts on — the current diff, the current state. Once you are \
asking when something changed, that is a Worker's errand.

A Worker's report is not final either. Ask for the check that would \
settle it, or put the same neutral question to two Workers and compare \
what comes back. Where nothing settled it, write that it is unverified. \
If comparing two answers means nothing else can proceed, say so — the \
size of the roster is the operator's to change, not a constraint to \
work around silently.

If you have consulted the Goinkyo, send_to_peer(to: \"goinkyo\") reaches \
it again while the consultation stays open — you are not limited to \
the request that started it. A consultation does not end by itself — \
you end it, with dismiss_goinkyo: an arbitration you already have your answer to is \
finished, and one left open is a session still running with nothing \
left to do. A design consultation lasting until the design is settled \
is the ordinary case, not that.

banto itself tells the operator when the whole brigade has gone quiet, so \
do not announce that you are done. When the operator alone can decide \
something AND work stops until they do, write the question in your own \
output and then call request_decision: it shows the operator that you are \
waiting, not the question, and it does not block you, so carry on with \
whatever can proceed. banto keeps showing it until the operator answers in \
your pane. Do not use it for status, for a question you or a Worker can \
settle, or for finished work. Use your own question picker only when no \
Worker is running: it stops you, and with Workers running request_decision \
is how the operator learns where you are stuck without stopping them.";

/// See [`BrigadeConfig::worker_prompt`].
const DEFAULT_WORKER_PROMPT: &str = "\
You are {token}, a Worker in banto brigade {brigade}, working under its \
Director in this same working directory. banto relays between you: the \
Director's instructions arrive through check_messages (banto nudges you \
when something is waiting), and send_to_peer is how you report back — \
findings, results, questions. The Director cannot see your pane, so put \
what matters in the message you send, not just in your own scrollback. Do \
not reply to a message that asks for nothing: a reply can wake the Director \
for a turn over its whole context.

Report what you checked and how you checked it. Say which source each \
claim about this repository rests on: `git show HEAD:<path>` for what is \
committed, and the working tree only for files the Director has asked \
you to edit or review — a tree can be mid-write, and reading one as if \
it were settled has misled this brigade before. Where you could not settle something, say it is unverified \
rather than closing the gap with a guess. Reasons travel with \
conclusions; the Director cannot see what you read. The work is yours \
to do — do not hand off to sub-agents.";

/// See [`BrigadeConfig::goinkyo_prompt`].
const DEFAULT_GOINKYO_PROMPT: &str = "\
You are the Goinkyo of banto brigade {brigade} — the retired elder this \
shop calls back in. You are not running the rest of the time, you hold \
nothing from before this moment, and you have no stake in anything \
already concluded here. That is the whole reason you were called.

The Director filed its request at {request} — read that first. Its \
first line says which of your two duties this is.

**Kind: arbitration.** The Director and a Worker disagree, or the \
Director is stuck. Read the source yourself: the Director's account of \
the disagreement is not the evidence, the Worker's own words and the \
code are. Where the two accounts differ, say which one the code \
supports; where neither does, say that instead of picking a side.

**Kind: design.** Nothing is built or committed to yet and the Director \
wants the shape, or the course, thought through first. Read what already exists around it — the \
constraints in the request are what the Director believes it is bound \
by, and those are checkable. Say where the proposed shape breaks, what \
it costs later that it does not cost now, and which of the discarded \
alternatives you would go back for. A design you would build the same \
way is a real answer; say so plainly rather than inventing an objection \
to justify having been asked.

Either way: if what you have is not enough to judge, say what is \
missing with send_to_peer and wait. You advise, you do not take the \
work over. The moment you start changing things you are a party to the \
argument rather than the one who can see it. Answer the Director with \
send_to_peer: say what you would do, why, and what would change your \
mind. Leave that answer legible in your own pane as well. The operator \
sees at most the first line of a brigade message; your pane is the only \
place the whole of your side reaches them.";

impl Default for BrigadeConfig {
    fn default() -> Self {
        Self {
            workers: 1,
            worker_model: "sonnet".to_string(),
            worker_effort: "high".to_string(),
            worker_autocompact: "250000".to_string(),
            worker_model_codex: "terra".to_string(),
            worker_effort_codex: "high".to_string(),
            worker_agent: WorkerAgentSetting::default(),
            worker_permission_mode: "auto".to_string(),
            relay: RelayMode::Auto,
            director_prompt: DEFAULT_DIRECTOR_PROMPT.to_string(),
            worker_prompt: DEFAULT_WORKER_PROMPT.to_string(),
            goinkyo_prompt: DEFAULT_GOINKYO_PROMPT.to_string(),
            goinkyo_agent: GoinkyoAgentSetting::default(),
            goinkyo_model: "opus".to_string(),
            goinkyo_effort: "xhigh".to_string(),
            goinkyo_permission_mode: "auto".to_string(),
            goinkyo_disallowed_tools: "Edit,Write,NotebookEdit".to_string(),
            claude: ClaudeBrigadeConfig::default(),
            codex: CodexBrigadeConfig::default(),
            legacy_keys: Vec::new(),
            goinkyo_consult: "design".to_string(),
            message_log: false,
            request_attention: true,
        }
    }
}

/// The error every setting that takes one of a fixed set of words gives for a
/// value that is none of them. It names the key, what the operator wrote (cut
/// short and escaped, so a pasted blob or a control character cannot flood or
/// break the line `toml` prints it on) and what the key takes. `toml` adds the
/// line, the column and the offending line.
fn not_accepted<E: serde::de::Error>(key: &str, given: &str, accepted: &str) -> E {
    const SHOWN_CHARS: usize = 40;
    let head: String = given.chars().take(SHOWN_CHARS).collect();
    let cut = if given.chars().count() > SHOWN_CHARS {
        "\u{2026}"
    } else {
        ""
    };
    E::custom(format!(
        "`{key}` does not take {head:?}{cut}; it takes {accepted}"
    ))
}

/// The one rule for every model and effort value, whichever path it arrives on
/// (the config file, a built-in, the picker; a Worker of either product or the
/// Goinkyo): trimmed, and if nothing is left no flag is passed; otherwise the
/// trimmed text is passed as it is, unvalidated. Text inside the value is the
/// operator's own. Every reader goes through this, so the paths cannot disagree
/// about what whitespace means.
pub fn flag_value(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// `[brigade] worker_autocompact` as the operator wrote it: a TOML integer
/// becomes its digits and a string stays as it is. Any other TOML type (a
/// float, a boolean, a list, a table) is `serde`'s own "invalid type" error,
/// since only an integer or a string can be a token count; a float of
/// `250000.0` is not turned into digits. The text is then held to
/// [`autocompact_reading`]: a value the flag does not take is refused here
/// instead of reaching a Worker's command line.
fn autocompact_setting<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Visitor;

    struct Written;

    impl Visitor<'_> for Written {
        type Value = String;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("an integer or a string")
        }

        fn visit_i64<E>(self, value: i64) -> Result<String, E> {
            Ok(value.to_string())
        }

        fn visit_u64<E>(self, value: u64) -> Result<String, E> {
            Ok(value.to_string())
        }

        fn visit_str<E>(self, value: &str) -> Result<String, E> {
            Ok(value.to_string())
        }
    }

    let written = deserializer.deserialize_any(Written)?;
    match autocompact_reading(&written) {
        AutocompactReading::Invalid => Err(not_accepted(
            "worker_autocompact",
            &written,
            "\"auto\", an integer from 100000 to 1000000, or \"\" (no flag)",
        )),
        AutocompactReading::Pass(_) | AutocompactReading::Empty => Ok(written),
    }
}

/// `[brigade] goinkyo_consult`: a string that is a level (or empty), kept as
/// written. Not a string is `serde`'s "invalid type" error.
fn consult_setting<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let written = String::deserialize(deserializer)?;
    match consult_reading(&written) {
        ConsultReading::Invalid => Err(not_accepted(
            "goinkyo_consult",
            &written,
            "\"off\", \"disputes\", \"design\", \"decisions\" or \"\" (no paragraph)",
        )),
        ConsultReading::Level(_) | ConsultReading::Empty => Ok(written),
    }
}

/// `agents`: `""`, `"all"`, or product names, kept as written. See
/// [`agents_are_valid`].
fn agents_setting<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let written = String::deserialize(deserializer)?;
    if agents_are_valid(&written) {
        Ok(written)
    } else {
        Err(not_accepted(
            "agents",
            &written,
            "\"all\", \"\", or a comma-separated list of \"claude\" and \"codex\"",
        ))
    }
}

/// `[keys] prefix`: a chord [`crate::engine::PrefixKey`] can read, kept as written.
fn prefix_setting<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let written = String::deserialize(deserializer)?;
    if crate::engine::PrefixKey::try_parse(&written).is_some() {
        Ok(written)
    } else {
        Err(not_accepted(
            "prefix",
            &written,
            "\"C-<char>\" for a Control chord (\"C-b\") or one character for a bare key",
        ))
    }
}

/// The window sizes `--autocompact` takes, in tokens, as the CLI's own help
/// states them (`auto, or 100k–1M tokens`).
const AUTOCOMPACT_TOKENS: std::ops::RangeInclusive<u64> = 100_000..=1_000_000;

/// How readily a Director consults the Goinkyo, from never to whenever it
/// chooses a course. See [`BrigadeConfig::goinkyo_consult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoinkyoConsult {
    /// Not on the Director's own initiative; the operator's request still is.
    Off,
    /// Only when the Director and a Worker disagree, or it is stuck.
    Disputes,
    /// Disputes, and before anything new is built. The built-in.
    Design,
    /// Design, and before the Director commits the brigade to a chosen course.
    Decisions,
}

enum ConsultReading {
    Level(GoinkyoConsult),
    /// Nothing written: no paragraph, the tool allowed.
    Empty,
    /// Written, but not a level. Refused when a file is read ([`consult_setting`]);
    /// only a `BrigadeConfig` built in code can hold one, and is read as the
    /// built-in.
    Invalid,
}

/// The one rule for `goinkyo_consult`, used by every reader: the UI process,
/// `banto _mcp` and `banto _hook` each load the config themselves and must not
/// disagree about what a value means. Trimmed and case-insensitive, like
/// `worker_autocompact`'s `auto`.
fn consult_reading(raw: &str) -> ConsultReading {
    let Some(text) = flag_value(raw) else {
        return ConsultReading::Empty;
    };
    match text.to_ascii_lowercase().as_str() {
        "off" => ConsultReading::Level(GoinkyoConsult::Off),
        "disputes" => ConsultReading::Level(GoinkyoConsult::Disputes),
        "design" => ConsultReading::Level(GoinkyoConsult::Design),
        "decisions" => ConsultReading::Level(GoinkyoConsult::Decisions),
        _ => ConsultReading::Invalid,
    }
}

enum AutocompactReading {
    /// What goes after `--autocompact`.
    Pass(String),
    /// Nothing written: the operator asked for no flag.
    Empty,
    /// Written, but not a value the flag takes. Refused when a file is read
    /// ([`autocompact_setting`]); only a `BrigadeConfig` built in code can hold
    /// one, and it passes nothing.
    Invalid,
}

/// The one rule for `worker_autocompact`. Plain ASCII digits only, so a sign, a
/// unit (`250k`), a separator or an exponent is invalid rather than guessed at;
/// the number is passed back as canonical decimal text, so leading zeros never
/// reach the CLI.
fn autocompact_reading(raw: &str) -> AutocompactReading {
    let Some(text) = flag_value(raw) else {
        return AutocompactReading::Empty;
    };
    if text.eq_ignore_ascii_case("auto") {
        return AutocompactReading::Pass("auto".to_string());
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return AutocompactReading::Invalid;
    }
    match text.parse::<u64>() {
        Ok(tokens) if AUTOCOMPACT_TOKENS.contains(&tokens) => {
            AutocompactReading::Pass(tokens.to_string())
        }
        _ => AutocompactReading::Invalid,
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
            BrigadeRole::Goinkyo => &self.goinkyo_prompt,
        };
        (!template.is_empty()).then_some(template.as_str())
    }

    /// The model a Worker running `agent` is created with, or `None` for "pass
    /// no model flag at all" (either product's own empty-string escape
    /// hatch). Read when a Worker's session is first created, and by the
    /// picker as its pre-filled text; never when one is resumed.
    pub fn worker_model_for(&self, agent: AgentKind) -> Option<&str> {
        let raw = match agent {
            AgentKind::ClaudeCode => {
                table_then_legacy(&self.claude.worker_model, &self.worker_model)
            }
            AgentKind::Codex => {
                table_then_legacy(&self.codex.worker_model, &self.worker_model_codex)
            }
        };
        flag_value(raw)
    }

    /// The effort a Worker running `agent` is created with, or `None` for
    /// "pass no effort flag at all". Same reading rule as
    /// [`Self::worker_model_for`].
    pub fn worker_effort_for(&self, agent: AgentKind) -> Option<&str> {
        let raw = match agent {
            AgentKind::ClaudeCode => {
                table_then_legacy(&self.claude.worker_effort, &self.worker_effort)
            }
            AgentKind::Codex => {
                table_then_legacy(&self.codex.worker_effort, &self.worker_effort_codex)
            }
        };
        flag_value(raw)
    }

    /// The value a Claude Worker's `--autocompact` gets: `auto`, or the
    /// token count as plain digits. `None` passes no flag: the setting is empty
    /// (or, only for a config built in code, invalid). Read on every launch of a
    /// Claude Worker, a resume included ([`Self::worker_autocompact`]'s field doc
    /// says why).
    pub fn worker_autocompact(&self) -> Option<String> {
        match autocompact_reading(&self.worker_autocompact) {
            AutocompactReading::Pass(value) => Some(value),
            AutocompactReading::Empty | AutocompactReading::Invalid => None,
        }
    }

    /// What a Worker of `agent` is launched with for this setting. Claude
    /// takes [`Self::worker_autocompact`] as it stands. Codex takes the token
    /// count only: `auto` is Claude's word, and Codex has no equivalent, so it
    /// gets no override at all (its own default threshold stays).
    pub fn worker_autocompact_for(&self, agent: AgentKind) -> Option<String> {
        let value = self.worker_autocompact()?;
        match agent {
            AgentKind::ClaudeCode => Some(value),
            AgentKind::Codex => (value != "auto").then_some(value),
        }
    }

    /// The level [`Self::goinkyo_consult`] names, or `None` for the empty
    /// string (no paragraph, the tool allowed). A value that is not a level
    /// cannot come from a file (it is refused); a config built in code reads it
    /// as the built-in `design`.
    pub fn goinkyo_consult(&self) -> Option<GoinkyoConsult> {
        match consult_reading(&self.goinkyo_consult) {
            ConsultReading::Level(level) => Some(level),
            ConsultReading::Empty => None,
            ConsultReading::Invalid => Some(GoinkyoConsult::Design),
        }
    }

    /// [`Self::worker_permission_mode`], or `None` for the empty-string
    /// escape hatch. No `agent` parameter, unlike [`Self::worker_model_for`]:
    /// Codex has no `--permission-mode` counterpart (`launch::AgentLaunch::
    /// Codex` carries no such field), so a Codex Worker simply never reaches
    /// anywhere this value would apply.
    pub fn worker_permission_mode(&self) -> Option<&str> {
        let raw = table_then_legacy(
            &self.claude.worker_permission_mode,
            &self.worker_permission_mode,
        );
        (!raw.is_empty()).then_some(raw)
    }

    /// [`Self::goinkyo_model`], or `None` for the empty-string escape hatch.
    /// The Claude Goinkyo's; [`Self::goinkyo_model_for`] picks by product.
    pub fn goinkyo_model(&self) -> Option<&str> {
        self.goinkyo_model_for(AgentKind::ClaudeCode)
    }

    /// [`Self::goinkyo_effort`], or `None` for the empty-string escape hatch.
    /// The Claude Goinkyo's; [`Self::goinkyo_effort_for`] picks by product.
    pub fn goinkyo_effort(&self) -> Option<&str> {
        self.goinkyo_effort_for(AgentKind::ClaudeCode)
    }

    /// The model a Goinkyo running `agent` is created with, or `None` for "pass
    /// no model flag". Claude's is `[brigade.claude] goinkyo_model`, else its
    /// legacy flat key, else `opus`; Codex's is `[brigade.codex] goinkyo_model`,
    /// else nothing (a Codex Goinkyo has no legacy key and no built-in). Never
    /// the other product's: a model word means nothing to the other binary.
    pub fn goinkyo_model_for(&self, agent: AgentKind) -> Option<&str> {
        let raw = match agent {
            AgentKind::ClaudeCode => {
                table_then_legacy(&self.claude.goinkyo_model, &self.goinkyo_model)
            }
            AgentKind::Codex => self.codex.goinkyo_model.as_deref().unwrap_or(""),
        };
        flag_value(raw)
    }

    /// The effort a Goinkyo running `agent` is created with, or `None`. Same
    /// reading rule as [`Self::goinkyo_model_for`].
    pub fn goinkyo_effort_for(&self, agent: AgentKind) -> Option<&str> {
        let raw = match agent {
            AgentKind::ClaudeCode => {
                table_then_legacy(&self.claude.goinkyo_effort, &self.goinkyo_effort)
            }
            AgentKind::Codex => self.codex.goinkyo_effort.as_deref().unwrap_or(""),
        };
        flag_value(raw)
    }

    /// [`Self::goinkyo_permission_mode`], or `None` for the empty-string
    /// escape hatch.
    pub fn goinkyo_permission_mode(&self) -> Option<&str> {
        let raw = table_then_legacy(
            &self.claude.goinkyo_permission_mode,
            &self.goinkyo_permission_mode,
        );
        (!raw.is_empty()).then_some(raw)
    }

    /// [`Self::goinkyo_disallowed_tools`], or `None` for the empty-string
    /// escape hatch.
    pub fn goinkyo_disallowed_tools(&self) -> Option<&str> {
        let raw = table_then_legacy(
            &self.claude.goinkyo_disallowed_tools,
            &self.goinkyo_disallowed_tools,
        );
        (!raw.is_empty()).then_some(raw)
    }

    /// What startup says about legacy `[brigade]` keys, or `None` when the file
    /// wrote none: the names, and how many of them the product table also sets and
    /// so overrides. Short and without the new spelling of each key (the README
    /// has that): the status line draws one unwrapped span and clips it at the
    /// terminal's width, so a sentence per key lost most of nine keys' worth, and
    /// a notice per key would hold the line for as many turns as the file has keys.
    pub fn legacy_key_notice(&self) -> Option<String> {
        const NAMED: usize = 3;
        let found: Vec<&LegacyKey> = LEGACY_BRIGADE_KEYS
            .iter()
            .filter(|key| self.legacy_keys.contains(&key.legacy))
            .collect();
        if found.is_empty() {
            return None;
        }
        let names: Vec<&str> = found.iter().take(NAMED).map(|key| key.legacy).collect();
        let more = found.len().saturating_sub(NAMED);
        let more = (more > 0).then(|| format!(" and {more} more"));
        let ignored = found.iter().filter(|key| self.table_sets(key)).count();
        let ignored = (ignored > 0).then(|| format!("; {ignored} ignored (the table wins)"));
        Some(format!(
            "config: legacy [brigade] keys ({}{}) moved to [brigade.claude]/[brigade.codex]; \
             see README{}",
            names.join(", "),
            more.unwrap_or_default(),
            ignored.unwrap_or_default()
        ))
    }

    /// Whether the product table sets what `key` is the legacy spelling of.
    fn table_sets(&self, key: &LegacyKey) -> bool {
        match key.legacy {
            "worker_model" => self.claude.worker_model.is_some(),
            "worker_effort" => self.claude.worker_effort.is_some(),
            "worker_permission_mode" => self.claude.worker_permission_mode.is_some(),
            "goinkyo_model" => self.claude.goinkyo_model.is_some(),
            "goinkyo_effort" => self.claude.goinkyo_effort.is_some(),
            "goinkyo_permission_mode" => self.claude.goinkyo_permission_mode.is_some(),
            "goinkyo_disallowed_tools" => self.claude.goinkyo_disallowed_tools.is_some(),
            "worker_model_codex" => self.codex.worker_model.is_some(),
            "worker_effort_codex" => self.codex.worker_effort.is_some(),
            _ => false,
        }
    }
}

/// A flat `[brigade]` key and the product table that took over its meaning.
struct LegacyKey {
    legacy: &'static str,
    /// The key's name inside the table: `worker_model_codex` became
    /// `worker_model` under `[brigade.codex]`.
    key: &'static str,
    table: &'static str,
}

/// Every legacy key, in the order the notice names them.
const LEGACY_BRIGADE_KEYS: [LegacyKey; 9] = [
    LegacyKey {
        legacy: "worker_model",
        key: "worker_model",
        table: "claude",
    },
    LegacyKey {
        legacy: "worker_effort",
        key: "worker_effort",
        table: "claude",
    },
    LegacyKey {
        legacy: "worker_permission_mode",
        key: "worker_permission_mode",
        table: "claude",
    },
    LegacyKey {
        legacy: "goinkyo_model",
        key: "goinkyo_model",
        table: "claude",
    },
    LegacyKey {
        legacy: "goinkyo_effort",
        key: "goinkyo_effort",
        table: "claude",
    },
    LegacyKey {
        legacy: "goinkyo_permission_mode",
        key: "goinkyo_permission_mode",
        table: "claude",
    },
    LegacyKey {
        legacy: "goinkyo_disallowed_tools",
        key: "goinkyo_disallowed_tools",
        table: "claude",
    },
    LegacyKey {
        legacy: "worker_model_codex",
        key: "worker_model",
        table: "codex",
    },
    LegacyKey {
        legacy: "worker_effort_codex",
        key: "worker_effort",
        table: "codex",
    },
];

/// Of the keys a file wrote under `[brigade]`, the legacy ones, in
/// [`LEGACY_BRIGADE_KEYS`]' order. Pure so that the edge that parses the file
/// only has to hand over names.
pub fn legacy_brigade_keys_in<'a>(keys: impl IntoIterator<Item = &'a str>) -> Vec<&'static str> {
    let written: Vec<&str> = keys.into_iter().collect();
    LEGACY_BRIGADE_KEYS
        .iter()
        .filter(|key| written.contains(&key.legacy))
        .map(|key| key.legacy)
        .collect()
}

/// The resolution every product-table setting follows: the table's value when
/// it is set (an empty string counts: it is the escape hatch), else the legacy
/// flat key, which holds the built-in when the file wrote neither.
fn table_then_legacy<'a>(table: &'a Option<String>, legacy: &'a str) -> &'a str {
    table.as_deref().unwrap_or(legacy)
}

/// The plain `[brigade]` keys: every key of [`BrigadeConfig`] that is neither a
/// product table nor a legacy flat spelling. The test
/// `the_key_lists_name_every_field_their_struct_has` fails to compile when a
/// field is added to [`BrigadeConfig`] and passes only when this list, the two
/// below and [`LEGACY_BRIGADE_KEYS`] still cover it.
const PLAIN_BRIGADE_KEYS: [&str; 11] = [
    "workers",
    "worker_agent",
    "worker_autocompact",
    "relay",
    "request_attention",
    "director_prompt",
    "goinkyo_consult",
    "worker_prompt",
    "goinkyo_prompt",
    "goinkyo_agent",
    "message_log",
];

/// The keys [`ClaudeBrigadeConfig`] has.
const CLAUDE_TABLE_KEYS: [&str; 7] = [
    "worker_model",
    "worker_effort",
    "worker_permission_mode",
    "goinkyo_model",
    "goinkyo_effort",
    "goinkyo_permission_mode",
    "goinkyo_disallowed_tools",
];

/// The keys [`CodexBrigadeConfig`] has.
const CODEX_TABLE_KEYS: [&str; 4] = [
    "worker_model",
    "worker_effort",
    "goinkyo_model",
    "goinkyo_effort",
];

/// Why `key`, written under `[brigade.<table>]`, would be lost, or `None` when it
/// is a key of that table or one nothing knows (which stays ignored, like every
/// unknown key). TOML ends `[brigade]` at the first `[brigade.…]` header, so a
/// plain `[brigade]` key written below one belongs to that table and no
/// product table reads it: `relay = "typo"` there is not refused as a typo, it
/// is not read at all.
fn misplaced_key(table: &str, table_keys: &[&str], key: &str) -> Option<String> {
    if PLAIN_BRIGADE_KEYS.contains(&key) {
        return Some(format!(
            "`{key}` is a [brigade] key, but it is written under [brigade.{table}]; \
             move it above the first [brigade.*] table"
        ));
    }
    if table_keys.contains(&key) {
        return None;
    }
    let legacy = LEGACY_BRIGADE_KEYS
        .iter()
        .find(|legacy| legacy.legacy == key)?;
    Some(if legacy.table == table {
        format!(
            "`{key}` is the old [brigade] spelling of `{}`, which is the name inside \
             [brigade.{table}]",
            legacy.key
        )
    } else {
        format!(
            "`{key}` is a legacy [brigade] key for `{}` under [brigade.{}], but it is \
             written under [brigade.{table}], where nothing reads it; write it there \
             instead",
            legacy.key, legacy.table
        )
    })
}

fn claude_table<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ClaudeBrigadeConfig, D::Error> {
    guarded_table("claude", &CLAUDE_TABLE_KEYS, deserializer)
}

fn codex_table<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<CodexBrigadeConfig, D::Error> {
    guarded_table("codex", &CODEX_TABLE_KEYS, deserializer)
}

/// Reads a product table as `T` would, refusing a key [`misplaced_key`] names.
/// The refusal is raised as the value's own error, inside the format's value
/// deserialization, so that `toml` gives it the line and column of the offending
/// value and prints that line, as it does for a value of the wrong type. A plain
/// `Deserialize` cannot do this: it would have to be told every key to refuse
/// and could not see the others.
fn guarded_table<'de, T, D>(
    table: &'static str,
    table_keys: &'static [&'static str],
    deserializer: D,
) -> Result<T, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, Visitor};
    use std::marker::PhantomData;

    struct Guard<T> {
        table: &'static str,
        table_keys: &'static [&'static str],
        kind: PhantomData<T>,
    }
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Guard<T> {
        type Value = T;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a table")
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            T::deserialize(de::value::MapAccessDeserializer::new(Watch {
                inner: map,
                table: self.table,
                table_keys: self.table_keys,
                refuse: None,
            }))
        }
    }

    struct Watch<A> {
        inner: A,
        table: &'static str,
        table_keys: &'static [&'static str],
        /// What the key just read is refused for, said when its value is asked for.
        refuse: Option<String>,
    }
    impl<'de, A: MapAccess<'de>> MapAccess<'de> for Watch<A> {
        type Error = A::Error;

        fn next_key_seed<K: DeserializeSeed<'de>>(
            &mut self,
            seed: K,
        ) -> Result<Option<K::Value>, A::Error> {
            let Some(key) = self.inner.next_key::<String>()? else {
                return Ok(None);
            };
            self.refuse = misplaced_key(self.table, self.table_keys, &key);
            seed.deserialize(key.into_deserializer()).map(Some)
        }

        fn next_value_seed<V: DeserializeSeed<'de>>(
            &mut self,
            seed: V,
        ) -> Result<V::Value, A::Error> {
            match self.refuse.take() {
                Some(why) => self.inner.next_value_seed(Refuse {
                    why,
                    value: PhantomData,
                }),
                None => self.inner.next_value_seed(seed),
            }
        }
    }

    /// Takes the place of the value's own seed and fails inside the value's
    /// deserialization.
    struct Refuse<T> {
        why: String,
        value: PhantomData<T>,
    }
    impl<'de, T> DeserializeSeed<'de> for Refuse<T> {
        type Value = T;

        fn deserialize<D: serde::Deserializer<'de>>(self, _: D) -> Result<T, D::Error> {
            Err(de::Error::custom(self.why))
        }
    }

    deserializer.deserialize_map(Guard {
        table,
        table_keys,
        kind: PhantomData::<T>,
    })
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

/// Emporium keybinding settings. Just the tmux-style prefix chord — full
/// user-remappable keymaps are out of scope (a scoped decision, not an
/// oversight: see `crate::engine`'s `PrefixKey`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct KeysConfig {
    /// The prefix chord for pane operations (tmux-style: press it, then a
    /// second key — see `crate::engine::PrefixAction` for what each one
    /// does). `"C-<char>"` for a Control chord (e.g. the default `"C-b"`),
    /// or a bare single character for an unmodified key; anything else is
    /// refused when the file is read. The chord grammar is
    /// `crate::engine::PrefixKey::try_parse`'s, which this field asks and then
    /// keeps the raw string.
    #[serde(deserialize_with = "prefix_setting")]
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
    pub activity: ActivityConfig,
    pub brigade: BrigadeConfig,
    pub keys: KeysConfig,
    pub agent_binaries: AgentBinaries,
    /// Which agent products banto discovers sessions for: `"all"` (also
    /// what an absent or empty string means, and the default), or a
    /// comma-separated list of product names (`claude`, `codex`), each
    /// case-insensitive and trimmed. A name that is not a product, an empty
    /// entry included, is refused when the file is read. Kept as the raw string
    /// and resolved by [`resolve_agents`].
    #[serde(deserialize_with = "agents_setting")]
    pub agents: String,
    /// Overrides the provider's default `~/.claude` location (read-only!).
    pub claude_home: Option<PathBuf>,
    /// Overrides the provider's default `~/.codex` location (read-only!).
    pub codex_home: Option<PathBuf>,
    /// Overrides `banto.db` in the data directory. May lie outside it, in which
    /// case the data-directory sweep does not run (it only trusts the database
    /// it owns). Never inside an agent home.
    pub db_path: Option<PathBuf>,
    /// Overrides `banto_io::data_dir::DataDir::default_root`: the directory the
    /// database, the MCP configs, the consultation requests and the message logs
    /// live in. Top level only: under a `[section]` header it is that table's
    /// unknown field and is ignored. Never inside an agent home.
    pub data_dir: Option<PathBuf>,
}

/// The result of resolving [`Config::agents`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedAgents {
    /// The agent products banto should actually discover sessions for.
    pub enabled: BTreeSet<AgentKind>,
}

/// The product a name in `agents` stands for, case-insensitive.
fn agent_named(name: &str) -> Option<AgentKind> {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(AgentKind::ClaudeCode),
        "codex" => Some(AgentKind::Codex),
        _ => None,
    }
}

/// Whether `raw` is a value `agents` takes: empty, `all` (case-insensitive), or
/// a comma-separated list of product names. One name that is not a product, an
/// empty entry (`"claude,"`) included, makes the whole value invalid.
fn agents_are_valid(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("all")
        || trimmed.split(',').all(|name| agent_named(name).is_some())
}

/// Resolve [`Config::agents`] into the set of agent products banto should
/// discover sessions for. Empty (including an absent field, which
/// deserializes to the empty string) or `"all"` (case-insensitive) resolves
/// to [`AgentKind::ALL`] — "all" means every product this build supports,
/// not a wildcard for products that don't exist yet. Otherwise each
/// comma-separated entry is looked up independently, matched
/// case-insensitively with surrounding whitespace trimmed.
///
/// Total, though a file can no longer reach the branches that make it so: a
/// value with a name that is not a product is refused when the file is read
/// ([`agents_setting`]). A `Config` built in code can still hold one; its
/// unrecognized names are dropped, and a setting with no recognized name at all
/// resolves to every product rather than to an empty set that would discover
/// nothing.
pub fn resolve_agents(raw: &str) -> ResolvedAgents {
    let recognized: BTreeSet<AgentKind> = raw.trim().split(',').filter_map(agent_named).collect();
    ResolvedAgents {
        enabled: if recognized.is_empty() {
            AgentKind::ALL.into_iter().collect()
        } else {
            recognized
        },
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

    /// `[brigade] worker_autocompact = <toml>` parsed, and what a Claude Worker
    /// would be handed for it.
    fn autocompact_of(toml_value: &str) -> Option<String> {
        parse(&format!("[brigade]\nworker_autocompact = {toml_value}\n"))
            .brigade
            .worker_autocompact()
    }

    #[test]
    fn worker_autocompact_for_a_codex_worker_is_the_token_count_and_never_auto() {
        for (written, claude, codex) in [
            ("250000", Some("250000"), Some("250000")),
            ("\"300000\"", Some("300000"), Some("300000")),
            ("100000", Some("100000"), Some("100000")),
            ("1000000", Some("1000000"), Some("1000000")),
            ("\"auto\"", Some("auto"), None),
            ("\"AUTO\"", Some("auto"), None),
            ("\"\"", None, None),
        ] {
            let config = parse(&format!("[brigade]\nworker_autocompact = {written}\n"));
            assert_eq!(
                config
                    .brigade
                    .worker_autocompact_for(AgentKind::ClaudeCode)
                    .as_deref(),
                claude,
                "Claude, {written}"
            );
            assert_eq!(
                config
                    .brigade
                    .worker_autocompact_for(AgentKind::Codex)
                    .as_deref(),
                codex,
                "Codex, {written}"
            );
        }
    }

    /// `[brigade] goinkyo_consult = <toml>` parsed.
    fn consult_of(toml_value: &str) -> BrigadeConfig {
        parse(&format!("[brigade]\ngoinkyo_consult = {toml_value}\n")).brigade
    }

    #[test]
    fn goinkyo_consult_defaults_to_design() {
        let brigade = BrigadeConfig::default();
        assert_eq!(brigade.goinkyo_consult(), Some(GoinkyoConsult::Design));
        assert_eq!(
            parse("[brigade]\nworkers = 2\n").brigade.goinkyo_consult(),
            Some(GoinkyoConsult::Design)
        );
    }

    #[test]
    fn goinkyo_consult_takes_each_level_by_name() {
        for (written, level) in [
            ("off", GoinkyoConsult::Off),
            ("disputes", GoinkyoConsult::Disputes),
            ("design", GoinkyoConsult::Design),
            ("decisions", GoinkyoConsult::Decisions),
            (" Decisions ", GoinkyoConsult::Decisions),
            ("OFF", GoinkyoConsult::Off),
        ] {
            let brigade = consult_of(&format!("\"{written}\""));
            assert_eq!(brigade.goinkyo_consult(), Some(level), "{written:?}");
        }
    }

    #[test]
    fn goinkyo_consult_empty_is_no_policy_at_all() {
        // The file's empty-string convention: the operator who writes his own
        // `director_prompt` keeps the policy text, and the tool stays allowed.
        for written in ["\"\"", "\"  \""] {
            let brigade = consult_of(written);
            assert_eq!(brigade.goinkyo_consult(), None, "{written}");
        }
    }

    #[test]
    fn a_goinkyo_consult_built_in_code_that_is_not_a_level_reads_as_the_built_in() {
        // A file cannot hold one (it is refused), but a `BrigadeConfig` can be
        // built by hand, and the reader stays total.
        let brigade = BrigadeConfig {
            goinkyo_consult: "sometimes".into(),
            ..BrigadeConfig::default()
        };
        assert_eq!(brigade.goinkyo_consult(), Some(GoinkyoConsult::Design));
    }

    #[test]
    fn worker_autocompact_defaults_to_the_operators_window() {
        let brigade = Config::default().brigade;
        assert_eq!(brigade.worker_autocompact().as_deref(), Some("250000"));
        // A file that never mentions the key gets the same.
        let config = parse("[brigade]\nworkers = 2\n");
        assert_eq!(
            config.brigade.worker_autocompact().as_deref(),
            Some("250000")
        );
    }

    #[test]
    fn worker_autocompact_takes_an_integer_or_a_string_and_passes_digits_or_auto() {
        for (written, passed) in [
            ("250000", "250000"),
            ("\"250000\"", "250000"),
            ("\"auto\"", "auto"),
            ("\"AUTO\"", "auto"),
            ("\" auto \"", "auto"),
            ("\" 300000 \"", "300000"),
            ("100000", "100000"),
            ("1000000", "1000000"),
            ("\"100000\"", "100000"),
            ("\"1000000\"", "1000000"),
            // Digits are read as a number, so leading zeros do not reach the CLI.
            ("\"0300000\"", "300000"),
        ] {
            assert_eq!(
                autocompact_of(written).as_deref(),
                Some(passed),
                "{written}"
            );
        }
    }

    #[test]
    fn worker_autocompact_empty_passes_nothing_and_is_not_an_error() {
        for written in ["\"\"", "\"   \""] {
            assert_eq!(autocompact_of(written), None, "{written}");
        }
    }

    #[test]
    fn a_worker_autocompact_built_in_code_outside_the_window_passes_nothing() {
        // Only a hand-built config can hold one: a file is refused.
        for written in ["99999", "1000001", "abc", "250k", "-1"] {
            let brigade = BrigadeConfig {
                worker_autocompact: written.into(),
                ..BrigadeConfig::default()
            };
            assert_eq!(brigade.worker_autocompact(), None, "{written}");
            assert_eq!(brigade.worker_autocompact_for(AgentKind::Codex), None);
        }
    }

    #[test]
    fn defaults_have_documented_values() {
        let config = Config::default();
        assert_eq!(config.activity.today_hours, 24);
        assert_eq!(config.activity.week_days, 7);
        assert_eq!(config.brigade.workers, 1);
        assert_eq!(config.brigade.worker_model, "sonnet");
        assert_eq!(config.brigade.worker_model_codex, "terra");
        assert_eq!(config.brigade.worker_agent, WorkerAgentSetting::Inherit);
        assert_eq!(config.brigade.relay, RelayMode::Auto);
        assert_eq!(config.keys.prefix, "C-b");
        assert_eq!(config.agents, "");
        assert_eq!(config.claude_home, None);
        assert_eq!(config.codex_home, None);
        assert_eq!(config.db_path, None);
        assert_eq!(config.data_dir, None);
    }

    #[test]
    fn keys_prefix_parses_from_toml() {
        let config = parse("[keys]\nprefix = \"C-a\"\n");
        assert_eq!(config.keys.prefix, "C-a");
    }

    #[test]
    fn keys_section_missing_yields_the_default_prefix() {
        let config = parse("[activity]\ntoday_hours = 12\n");
        assert_eq!(config.keys.prefix, "C-b");
    }

    #[test]
    fn partial_brigade_section_fills_remaining_defaults() {
        let config = parse("[brigade]\nworkers = 3\n");
        assert_eq!(config.brigade.workers, 3);
        assert_eq!(config.brigade.worker_count(), 3);
        assert_eq!(config.brigade.worker_model, "sonnet");
        assert_eq!(config.brigade.worker_model_codex, "terra");
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
        assert_eq!(config.brigade.worker_model_codex, "terra");
        assert_eq!(config.brigade.worker_agent, WorkerAgentSetting::Inherit);
    }

    #[test]
    fn worker_model_codex_defaults_independently_of_worker_model() {
        let config = parse("[brigade]\nworker_model = \"opus\"\n");
        assert_eq!(config.brigade.worker_model, "opus");
        assert_eq!(
            config.brigade.worker_model_codex, "terra",
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
        assert_eq!(brigade.worker_model_for(AgentKind::Codex), Some("terra"));
    }

    #[test]
    fn built_in_model_and_effort_defaults_name_no_version() {
        let brigade = BrigadeConfig::default();
        assert_eq!(brigade.worker_model, "sonnet");
        assert_eq!(brigade.worker_effort, "high");
        assert_eq!(brigade.worker_model_codex, "terra");
        assert_eq!(brigade.worker_effort_codex, "high");
        assert_eq!(brigade.goinkyo_model, "opus");
        assert_eq!(brigade.goinkyo_effort, "xhigh");
        // A pinned version is how a built-in goes stale (the Codex one named
        // a build that later stopped being the newest of its family): none of
        // the model words may carry a digit.
        for model in [
            &brigade.worker_model,
            &brigade.worker_model_codex,
            &brigade.goinkyo_model,
        ] {
            assert!(
                !model.chars().any(|c| c.is_ascii_digit()),
                "a built-in model must be a family word, not a version: {model:?}"
            );
        }
    }

    #[test]
    fn a_default_is_the_config_value_if_set_else_the_built_in() {
        // Absent keys: every one is the built-in, whatever else the file sets.
        let absent = parse(
            "[brigade]
workers = 2
",
        );
        assert_eq!(
            absent.brigade.worker_effort_for(AgentKind::ClaudeCode),
            Some("high")
        );
        assert_eq!(
            absent.brigade.worker_effort_for(AgentKind::Codex),
            Some("high")
        );
        assert_eq!(
            absent.brigade.worker_model_for(AgentKind::Codex),
            Some("terra")
        );

        // Set keys win, each product's independently of the other's.
        let set = parse(
            "[brigade]
worker_model = \"opus\"
worker_effort = \"low\"
             worker_model_codex = \"gpt-6-astra\"
worker_effort_codex = \"xhigh\"
",
        );
        assert_eq!(
            set.brigade.worker_model_for(AgentKind::ClaudeCode),
            Some("opus")
        );
        assert_eq!(
            set.brigade.worker_effort_for(AgentKind::ClaudeCode),
            Some("low")
        );
        assert_eq!(
            set.brigade.worker_model_for(AgentKind::Codex),
            Some("gpt-6-astra")
        );
        assert_eq!(
            set.brigade.worker_effort_for(AgentKind::Codex),
            Some("xhigh")
        );

        // Setting one product's key leaves the other product's built-in alone.
        let one = parse(
            "[brigade]
worker_effort_codex = \"low\"
",
        );
        assert_eq!(
            one.brigade.worker_effort_for(AgentKind::ClaudeCode),
            Some("high")
        );
        assert_eq!(one.brigade.worker_effort_for(AgentKind::Codex), Some("low"));
    }

    #[test]
    fn an_explicitly_empty_effort_passes_no_flag_for_that_product_only() {
        let cleared = parse(
            "[brigade]
worker_effort = \"\"
",
        );
        assert_eq!(
            cleared.brigade.worker_effort_for(AgentKind::ClaudeCode),
            None
        );
        assert_eq!(
            cleared.brigade.worker_effort_for(AgentKind::Codex),
            Some("high")
        );
        let cleared = parse(
            "[brigade]
worker_effort_codex = \"\"
",
        );
        assert_eq!(cleared.brigade.worker_effort_for(AgentKind::Codex), None);
        assert_eq!(
            cleared.brigade.worker_effort_for(AgentKind::ClaudeCode),
            Some("high")
        );
    }

    #[test]
    fn every_model_and_effort_value_is_trimmed_and_nothing_left_passes_no_flag() {
        // Whitespace-only for each of the six keys: no flag.
        let blank = parse(
            "[brigade]\nworker_model = \"  \"\nworker_effort = \"\\t\"\n\
             worker_model_codex = \" \"\nworker_effort_codex = \"  \"\n\
             goinkyo_model = \"   \"\ngoinkyo_effort = \" \"\n",
        );
        let b = &blank.brigade;
        assert_eq!(b.worker_model_for(AgentKind::ClaudeCode), None);
        assert_eq!(b.worker_effort_for(AgentKind::ClaudeCode), None);
        assert_eq!(b.worker_model_for(AgentKind::Codex), None);
        assert_eq!(b.worker_effort_for(AgentKind::Codex), None);
        assert_eq!(b.goinkyo_model(), None);
        assert_eq!(b.goinkyo_effort(), None);

        // Padded: the trimmed text, otherwise as given.
        let padded = parse(
            "[brigade]\nworker_model = \" opus \"\nworker_effort = \"\\tlow\"\n\
             worker_model_codex = \"  terra\"\nworker_effort_codex = \"xhigh  \"\n\
             goinkyo_model = \" haiku\"\ngoinkyo_effort = \"max \"\n",
        );
        let p = &padded.brigade;
        assert_eq!(p.worker_model_for(AgentKind::ClaudeCode), Some("opus"));
        assert_eq!(p.worker_effort_for(AgentKind::ClaudeCode), Some("low"));
        assert_eq!(p.worker_model_for(AgentKind::Codex), Some("terra"));
        assert_eq!(p.worker_effort_for(AgentKind::Codex), Some("xhigh"));
        assert_eq!(p.goinkyo_model(), Some("haiku"));
        assert_eq!(p.goinkyo_effort(), Some("max"));
        // Text inside the value is the operator's, untouched.
        let inner = parse("[brigade]\nworker_model = \"a b\"\n");
        assert_eq!(
            inner.brigade.worker_model_for(AgentKind::ClaudeCode),
            Some("a b")
        );
    }

    // -- WorkerAgentSetting ----------------------------------------------

    #[test]
    fn worker_agent_defaults_to_inherit() {
        assert_eq!(WorkerAgentSetting::default(), WorkerAgentSetting::Inherit);
        assert_eq!(WorkerAgentSetting::parse(""), None, "empty is not a value");
    }

    #[test]
    fn worker_agent_every_known_value_parses() {
        for (text, expected) in [
            ("inherit", WorkerAgentSetting::Inherit),
            ("select", WorkerAgentSetting::Select),
            ("claude", WorkerAgentSetting::Claude),
            ("codex", WorkerAgentSetting::Codex),
        ] {
            assert_eq!(WorkerAgentSetting::parse(text), Some(expected));
            let config = parse(&format!("[brigade]\nworker_agent = \"{text}\"\n"));
            assert_eq!(config.brigade.worker_agent, expected);
        }
    }

    #[test]
    fn worker_agent_is_case_insensitive_and_trims_whitespace() {
        for text in ["Codex", "CODEX", "  codex  "] {
            assert_eq!(
                WorkerAgentSetting::parse(text),
                Some(WorkerAgentSetting::Codex)
            );
        }
    }

    // -- GoinkyoAgentSetting ---------------------------------------------

    #[test]
    fn goinkyo_agent_defaults_to_inherit() {
        assert_eq!(GoinkyoAgentSetting::default(), GoinkyoAgentSetting::Inherit);
        assert_eq!(
            BrigadeConfig::default().goinkyo_agent,
            GoinkyoAgentSetting::Inherit
        );
        assert_eq!(
            parse(
                "[brigade]
workers = 2
"
            )
            .brigade
            .goinkyo_agent,
            GoinkyoAgentSetting::Inherit,
            "a file written before the key existed takes the built-in"
        );
    }

    #[test]
    fn goinkyo_agent_every_known_value_parses() {
        for (text, expected) in [
            ("inherit", GoinkyoAgentSetting::Inherit),
            ("claude", GoinkyoAgentSetting::Claude),
            ("codex", GoinkyoAgentSetting::Codex),
        ] {
            assert_eq!(GoinkyoAgentSetting::parse(text), Some(expected));
            let config = parse(&format!(
                "[brigade]
goinkyo_agent = \"{text}\"
"
            ));
            assert_eq!(config.brigade.goinkyo_agent, expected);
        }
    }

    #[test]
    fn goinkyo_agent_is_case_insensitive_and_trims_whitespace() {
        for text in ["Codex", "CODEX", "  codex  "] {
            assert_eq!(
                GoinkyoAgentSetting::parse(text),
                Some(GoinkyoAgentSetting::Codex)
            );
        }
        for text in ["Inherit", "INHERIT", "  inherit  "] {
            assert_eq!(
                GoinkyoAgentSetting::parse(text),
                Some(GoinkyoAgentSetting::Inherit)
            );
        }
    }

    #[test]
    fn goinkyo_agent_refuses_a_value_that_is_not_one_of_its_three() {
        for bad in [
            "\"made-up-product\"",
            "\"\"",
            "\"  \"",
            "\"inherits\"",
            "\"select\"",
        ] {
            assert_refused_naming(
                &format!(
                    "[brigade]
goinkyo_agent = {bad}
"
                ),
                "goinkyo_agent",
                bad.trim_matches('"'),
                &["inherit", "claude", "codex"],
            );
        }
        assert_wrong_type(
            "[brigade]
goinkyo_agent = 5
",
            "a string",
        );
        assert_eq!(GoinkyoAgentSetting::parse(""), None, "empty is not a value");
    }

    #[test]
    fn the_resolver_covers_every_setting_for_every_directors_product() {
        use AgentKind::{ClaudeCode, Codex};
        use GoinkyoAgentSetting::{Claude, Codex as CodexSetting, Inherit};
        for (setting, director, expected) in [
            (Inherit, ClaudeCode, ClaudeCode),
            (Inherit, Codex, Codex),
            (Claude, ClaudeCode, ClaudeCode),
            (Claude, Codex, ClaudeCode),
            (CodexSetting, ClaudeCode, Codex),
            (CodexSetting, Codex, Codex),
        ] {
            assert_eq!(
                resolve_goinkyo_agent(setting, director),
                expected,
                "{setting:?} under a {director:?} Director"
            );
        }
    }

    fn only(agent: AgentKind) -> BTreeSet<AgentKind> {
        BTreeSet::from([agent])
    }

    #[test]
    fn a_goinkyo_refusal_is_the_first_unmet_condition_in_the_order_it_is_fixed() {
        use AgentKind::{ClaudeCode, Codex};
        let both: BTreeSet<AgentKind> = AgentKind::ALL.into_iter().collect();
        let hook = |launchable, primed| CodexHookFacts { launchable, primed };
        // Nothing wrong: a Claude Goinkyo never asks about Codex's hook, and a
        // Codex one needs it launchable and primed.
        assert_eq!(
            goinkyo_refusal(ClaudeCode, &both, true, hook(false, false)),
            None
        );
        assert_eq!(goinkyo_refusal(Codex, &both, true, hook(true, true)), None);
        assert_eq!(
            goinkyo_refusal(Codex, &both, true, hook(true, false)),
            Some(GoinkyoRefusal::CodexHookNotPrimed)
        );
        // A path the hook cannot carry outranks the trust record: trusted or
        // not, the hook never runs.
        for primed in [false, true] {
            assert_eq!(
                goinkyo_refusal(Codex, &both, true, hook(false, primed)),
                Some(GoinkyoRefusal::CodexHookNotLaunchable)
            );
        }
        // Left out of `agents`.
        assert_eq!(
            goinkyo_refusal(Codex, &only(ClaudeCode), true, hook(true, true)),
            Some(GoinkyoRefusal::NotEnabled(Codex))
        );
        assert_eq!(
            goinkyo_refusal(ClaudeCode, &only(Codex), true, hook(true, true)),
            Some(GoinkyoRefusal::NotEnabled(ClaudeCode))
        );
        // A home that did not resolve outranks the hook, which lives in it.
        assert_eq!(
            goinkyo_refusal(Codex, &both, false, hook(false, false)),
            Some(GoinkyoRefusal::HomeUnresolved(Codex))
        );
        assert_eq!(
            goinkyo_refusal(ClaudeCode, &both, false, hook(false, false)),
            Some(GoinkyoRefusal::HomeUnresolved(ClaudeCode))
        );
        // Not enabled outranks everything after it.
        assert_eq!(
            goinkyo_refusal(Codex, &only(ClaudeCode), false, hook(false, false)),
            Some(GoinkyoRefusal::NotEnabled(Codex))
        );
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
        // The tool that names a Worker is mentioned, and the placeholders the
        // renderer substitutes are the same two as before: no new one.
        assert!(director.contains("name_member"));
        assert!(director.starts_with("You are the Director of banto brigade {brigade}."));
        assert_eq!(director.matches("{brigade}").count(), 1);
        assert_eq!(director.matches("{peers}").count(), 1);
        assert!(!director.contains("{token}") && !director.contains("{name}"));

        let worker = config.brigade.prompt_for(BrigadeRole::Worker).unwrap();
        assert!(worker.contains("{brigade}") && worker.contains("{token}"));
        assert!(worker.contains("send_to_peer") && worker.contains("check_messages"));
    }

    #[test]
    fn the_default_worker_prompt_says_where_each_claim_rests_and_allows_the_tree_for_files_the_director_named()
     {
        // Pins the two things the sentence must keep saying. An absolute ban on
        // the working tree made a Codex Worker refuse to review an uncommitted
        // change a Director had pinned by hash: Codex receives the briefing as
        // developer instructions, so even the operator's typed words could not
        // lift it, and reviewing before committing is how a brigade works.
        let config = Config::default();
        let worker = config.brigade.prompt_for(BrigadeRole::Worker).unwrap();
        let prompt = worker.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            prompt.contains("Say which source each claim about this repository rests on"),
            "{prompt}"
        );
        assert!(
            prompt.contains("`git show HEAD:<path>` for what is committed"),
            "committed state is read from HEAD: {prompt}"
        );
        assert!(
            prompt.contains(
                "the working tree only for files the Director has asked you to edit or review"
            ),
            "the tree is allowed for what the Director asked for: {prompt}"
        );
        assert!(
            !prompt.contains("never on the working tree"),
            "no absolute ban: {prompt}"
        );
    }

    #[test]
    fn the_default_director_briefing_prices_a_message_and_sends_a_worker_only_work() {
        let prompt = Config::default()
            .brigade
            .director_prompt
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        // The price is conditional on the relay, which is on unless configured off.
        assert_eq!(Config::default().brigade.relay, RelayMode::Auto);
        assert!(
            prompt.contains("With banto's relay on, which is the default"),
            "the relay condition is stated: {prompt}"
        );
        assert!(
            prompt.contains("wakes an idle recipient for a turn over its whole context"),
            "the price of a message is stated: {prompt}"
        );
        assert!(
            !prompt.contains("Every message wakes"),
            "a wake is not unconditional: {prompt}"
        );
        // And it is not guaranteed, so a sent message is not a read one.
        assert!(
            prompt.to_lowercase().contains("a wake is not guaranteed"),
            "no promise of delivery: {prompt}"
        );
        assert!(prompt.contains("member is busy; one nobody takes is shown as held"));
        assert!(!prompt.contains("a draft in a pane holds"));
        assert!(
            prompt.contains("only work, or a correction to work in flight"),
            "{prompt}"
        );
        // A stop is allowed whenever work may still be running: a Worker can send
        // an interim report or a question and keep going, so a report does not
        // mean it is finished.
        assert!(
            prompt.contains("and a stop whenever its work may still be running"),
            "{prompt}"
        );
        // The prohibition is for a Worker whose errand is finished and reported.
        assert!(
            prompt.contains(
                "Thanks, a stand-down and a closing note go to no Worker whose errand \
                 is finished and reported"
            ),
            "{prompt}"
        );
        assert!(
            prompt.contains("not even when the operator ends the day"),
            "{prompt}"
        );
        assert!(
            !prompt.contains("has already reported") && !prompt.contains("nothing in flight"),
            "a report alone is not read as the end of the work: {prompt}"
        );
        assert!(
            prompt.contains("banto tells the operator when the brigade has gone quiet"),
            "{prompt}"
        );
        // brigade_status says idle for a Codex Worker too, but from its turn record
        // alone (background work is invisible there), so the rule does not rest on it.
        assert!(
            !prompt.contains("shows in brigade_status"),
            "no claim about what brigade_status shows of a Worker: {prompt}"
        );
    }

    #[test]
    fn the_default_worker_briefing_says_not_to_reply_to_a_message_that_asks_for_nothing() {
        let prompt = Config::default()
            .brigade
            .worker_prompt
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            prompt.contains("Do not reply to a message that asks for nothing"),
            "{prompt}"
        );
        assert!(
            prompt.contains("a reply can wake the Director for a turn over its whole context"),
            "the reason is the price, and it is conditional: {prompt}"
        );
        assert!(!prompt.contains("each reply wakes"), "{prompt}");
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
    fn the_default_director_briefing_names_request_decision_and_says_when_to_use_it() {
        let prompt = &Config::default().brigade.director_prompt;
        // The tool by name, and what it is for: the operator alone can decide it
        // AND work stops until they do, with the question written in its own pane.
        assert!(prompt.contains("request_decision"), "{prompt}");
        assert!(
            prompt.contains("write the question in your own output and then call request_decision"),
            "{prompt}"
        );
        assert!(prompt.contains("shows the operator that you are waiting, not the question"));
        assert!(prompt.contains("the operator alone can decide"), "{prompt}");
        assert!(prompt.contains("work stops"), "{prompt}");
        // banto itself says when the brigade has gone quiet: no announcing.
        assert!(
            prompt.contains("banto itself tells the operator"),
            "{prompt}"
        );
        assert!(prompt.contains("gone quiet"), "{prompt}");
        assert!(prompt.contains("do not announce"), "{prompt}");
        // The Director's own question picker is for when no Worker is running.
        assert!(
            prompt.contains("only when no Worker is running"),
            "{prompt}"
        );
        // It does not block, and it is not for status or what can be settled.
        assert!(prompt.contains("does not block you"), "{prompt}");
        assert!(prompt.contains("status"), "{prompt}");
        // And it still renders: nothing in it is a placeholder.
        assert!(
            !prompt
                .replace("{brigade}", "")
                .replace("{peers}", "")
                .contains('{')
        );
    }

    #[test]
    fn a_requested_decision_is_an_attention_event_unless_the_config_turns_it_off() {
        assert!(Config::default().brigade.request_attention);
        assert!(parse("").brigade.request_attention);
        assert!(parse("[brigade]\nworkers = 2\n").brigade.request_attention);
        assert!(
            !parse("[brigade]\nrequest_attention = false\n")
                .brigade
                .request_attention
        );
        assert!(
            parse("[brigade]\nrequest_attention = true\n")
                .brigade
                .request_attention
        );
    }

    #[test]
    fn the_brigade_message_log_is_off_unless_the_config_turns_it_on() {
        assert!(!Config::default().brigade.message_log);
        assert!(!parse("").brigade.message_log);
        assert!(!parse("[brigade]\nworkers = 2\n").brigade.message_log);
        assert!(parse("[brigade]\nmessage_log = true\n").brigade.message_log);
    }

    #[test]
    fn brigade_goinkyo_prompt_defaults_to_something_that_substitutes_and_names_the_tool() {
        let config = parse("");
        let goinkyo = config.brigade.prompt_for(BrigadeRole::Goinkyo).unwrap();
        assert!(goinkyo.contains("{brigade}"));
        assert!(goinkyo.contains("{request}"));
        assert!(goinkyo.contains("send_to_peer"));
    }

    #[test]
    fn brigade_goinkyo_model_effort_permission_mode_and_disallowed_tools_default_and_have_their_own_empty_escape_hatch()
     {
        let config = Config::default();
        assert_eq!(config.brigade.goinkyo_model(), Some("opus"));
        assert_eq!(config.brigade.goinkyo_effort(), Some("xhigh"));
        assert_eq!(config.brigade.goinkyo_permission_mode(), Some("auto"));
        assert_eq!(
            config.brigade.goinkyo_disallowed_tools(),
            Some("Edit,Write,NotebookEdit")
        );

        let cleared = parse(
            "[brigade]\ngoinkyo_model = \"\"\ngoinkyo_effort = \"\"\n\
             goinkyo_permission_mode = \"\"\ngoinkyo_disallowed_tools = \"\"\n",
        );
        assert_eq!(cleared.brigade.goinkyo_model(), None);
        assert_eq!(cleared.brigade.goinkyo_effort(), None);
        assert_eq!(cleared.brigade.goinkyo_permission_mode(), None);
        assert_eq!(cleared.brigade.goinkyo_disallowed_tools(), None);

        let overridden = parse(
            "[brigade]\ngoinkyo_model = \"opus\"\ngoinkyo_effort = \"low\"\n\
             goinkyo_permission_mode = \"plan\"\ngoinkyo_disallowed_tools = \"Bash\"\n",
        );
        assert_eq!(overridden.brigade.goinkyo_model(), Some("opus"));
        assert_eq!(overridden.brigade.goinkyo_effort(), Some("low"));
        assert_eq!(overridden.brigade.goinkyo_permission_mode(), Some("plan"));
        assert_eq!(overridden.brigade.goinkyo_disallowed_tools(), Some("Bash"));
    }

    #[test]
    fn an_existing_config_toml_without_goinkyo_model_effort_permission_mode_or_disallowed_tools_gets_the_defaults()
     {
        // Same reasoning as `an_existing_config_toml_without_goinkyo_prompt_
        // gets_the_default`: a file written before these fields existed
        // must keep loading with their own defaults, not an empty value.
        let config = parse("[brigade]\nworker_model = \"sonnet\"\n");
        assert_eq!(config.brigade.goinkyo_model(), Some("opus"));
        assert_eq!(config.brigade.goinkyo_effort(), Some("xhigh"));
        assert_eq!(config.brigade.goinkyo_permission_mode(), Some("auto"));
        assert_eq!(
            config.brigade.goinkyo_disallowed_tools(),
            Some("Edit,Write,NotebookEdit")
        );
    }

    #[test]
    fn brigade_worker_permission_mode_defaults_to_auto_and_has_its_own_empty_escape_hatch() {
        let config = Config::default();
        assert_eq!(config.brigade.worker_permission_mode(), Some("auto"));

        let cleared = parse("[brigade]\nworker_permission_mode = \"\"\n");
        assert_eq!(cleared.brigade.worker_permission_mode(), None);

        let overridden = parse("[brigade]\nworker_permission_mode = \"plan\"\n");
        assert_eq!(overridden.brigade.worker_permission_mode(), Some("plan"));
    }

    #[test]
    fn an_existing_config_toml_without_worker_permission_mode_gets_the_default() {
        // Same reasoning as `an_existing_config_toml_without_goinkyo_model_
        // effort_permission_mode_or_disallowed_tools_gets_the_defaults`: a
        // file written before this field existed must keep loading with its
        // own default, not an empty value.
        let config = parse("[brigade]\nworker_model = \"sonnet\"\n");
        assert_eq!(config.brigade.worker_permission_mode(), Some("auto"));
    }

    #[test]
    fn brigade_goinkyo_prompt_is_overridable_and_an_empty_one_launches_with_no_flag() {
        let overridden = parse("[brigade]\ngoinkyo_prompt = \"advise on {brigade}\"\n");
        assert_eq!(
            overridden.brigade.prompt_for(BrigadeRole::Goinkyo),
            Some("advise on {brigade}")
        );

        let cleared = parse("[brigade]\ngoinkyo_prompt = \"\"\n");
        assert_eq!(cleared.brigade.prompt_for(BrigadeRole::Goinkyo), None);
    }

    #[test]
    fn an_existing_config_toml_without_goinkyo_prompt_gets_the_default() {
        // A file written before `goinkyo_prompt` existed must keep loading:
        // the new field silently takes its own default rather than erroring
        // or leaving the Goinkyo briefing empty.
        let config = parse("[brigade]\nworker_model = \"sonnet\"\n");
        assert_eq!(config.brigade.goinkyo_prompt, DEFAULT_GOINKYO_PROMPT);
    }

    #[test]
    fn partial_toml_fills_remaining_defaults() {
        let config = parse("agents = \"codex\"\n");
        assert_eq!(config.agents, "codex");
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
    fn unknown_keys_are_ignored() {
        let config = parse("future_option = true\n[some_new_section]\nx = 1\n");
        assert_eq!(config, Config::default());
    }

    /// `opener` was a setting once (how a session was opened outside banto's own
    /// panes). A config file that still has it, as a key or as a section, in any
    /// shape — a value it used to reject included — is an unknown key like any
    /// other: no error, and nothing else in the file is affected.
    #[test]
    fn a_retired_opener_setting_is_ignored_in_every_shape() {
        for text in [
            "opener = \"psmux\"\n",
            "opener = \"in-place\"\n",
            "opener = \"no-such-backend\"\n",
            "opener = 3\n",
            "[opener]\nmode = \"psmux\"\n",
            "[opener]\n",
            "[[opener]]\nbackend = \"tmux\"\n",
        ] {
            assert_eq!(parse(text), Config::default(), "{text:?}");
        }
        let config = parse("[opener.extra]\nx = 1\n[activity]\ntoday_hours = 12\n");
        assert_eq!(config.activity.today_hours, 12);
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
    fn data_dir_is_a_top_level_key_and_means_nothing_under_a_section() {
        let config = parse(
            "data_dir = \"C:/synthetic/banto-data\"
",
        );
        assert_eq!(
            config.data_dir,
            Some(PathBuf::from("C:/synthetic/banto-data"))
        );
        // Written after a `[section]` header the key belongs to that table, which
        // reads it as an unknown field: ignored, not an error, and not the data
        // directory. The README warns about it, so this is what it relies on.
        let config = parse(
            "[brigade]
data_dir = \"C:/synthetic/banto-data\"
",
        );
        assert_eq!(config.data_dir, None);
    }

    #[test]
    fn wrong_field_type_is_a_parse_error() {
        let result: Result<Config, _> = toml::from_str("claude_home = 5\n");
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
    }

    #[test]
    fn resolve_agents_all_is_case_insensitive_and_trims_whitespace() {
        for text in ["all", "ALL", "All", "  all  "] {
            let resolved = resolve_agents(text);
            assert_eq!(resolved.enabled, all_agents());
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
    fn resolve_agents_stays_total_for_a_config_built_in_code() {
        // A file with such a value is refused ([`agents_setting`]); a hand-built
        // `Config` can still hold one, and discovers something rather than nothing.
        assert_eq!(
            resolve_agents("claude,made-up-product").enabled,
            BTreeSet::from([AgentKind::ClaudeCode])
        );
        assert_eq!(resolve_agents("made-up-product").enabled, all_agents());
        assert_eq!(
            resolve_agents("nonsense,also-nonsense").enabled,
            all_agents()
        );
    }

    #[test]
    fn resolve_agents_a_repeated_name_still_yields_one_entry() {
        assert_eq!(
            resolve_agents("codex,codex").enabled,
            BTreeSet::from([AgentKind::Codex])
        );
    }

    // -- a value no setting takes stops banto -------------------------------
    //
    // Every setting that reads one of a fixed set of words refuses a value that
    // is not one of them, as it does a value of the wrong TOML type: a typo that
    // silently became the built-in ran the operator's agents on something they
    // did not choose. Trimming and case, where a setting had them, are kept; a
    // free-text setting (a model, an effort, a prompt) is not in this list
    // because it never had a fixed set.

    /// What `toml` says about `text` as a `Config`, or `None` when it loads.
    fn refusal(text: &str) -> Option<String> {
        toml::from_str::<Config>(text)
            .err()
            .map(|error| error.to_string())
    }

    /// `text` is refused by `key`'s own reason sentence, not by some other `toml`
    /// error: `toml` echoes the offending source line, so the key and the value
    /// would be in any message about that line. The sentence is checked whole
    /// (`` `key` does not take "given"; it takes ... ``) and for each accepted word.
    fn assert_refused_naming(text: &str, key: &str, given: &str, accepted: &[&str]) {
        let message = refusal(text).unwrap_or_else(|| panic!("{text:?} loaded"));
        assert!(
            message.starts_with("TOML parse error at line "),
            "{message}"
        );
        let reason = message
            .lines()
            .find(|line| line.starts_with('`'))
            .unwrap_or_else(|| panic!("no reason sentence in {message}"));
        let lead = format!("`{key}` does not take {given:?}; it takes ");
        assert!(reason.starts_with(&lead), "{lead} in {reason}");
        for word in accepted {
            assert!(reason.contains(word), "{word} in {reason}");
        }
    }

    /// `text` is refused for having the wrong TOML type, with `toml`'s own
    /// "invalid type ... expected `expected`" reason.
    fn assert_wrong_type(text: &str, expected: &str) {
        let message = refusal(text).unwrap_or_else(|| panic!("{text:?} loaded"));
        assert!(
            message.contains("invalid type: ") && message.contains(&format!("expected {expected}")),
            "{text:?}: {message}"
        );
    }

    #[test]
    fn relay_refuses_a_value_that_is_not_auto_or_manual() {
        for bad in ["\"typo\"", "\"\"", "\"  \"", "\"manual!\""] {
            assert_refused_naming(
                &format!("[brigade]\nrelay = {bad}\n"),
                "relay",
                bad.trim_matches('"'),
                &["auto", "manual"],
            );
        }
        for (ok, mode) in [
            ("auto", RelayMode::Auto),
            ("manual", RelayMode::Manual),
            (" Manual ", RelayMode::Manual),
            ("AUTO", RelayMode::Auto),
        ] {
            assert_eq!(
                parse(&format!("[brigade]\nrelay = \"{ok}\"\n"))
                    .brigade
                    .relay,
                mode
            );
        }
    }

    #[test]
    fn worker_agent_refuses_a_value_that_is_not_one_of_its_four() {
        for bad in ["\"typo\"", "\"\"", "\"  \"", "\"claud\""] {
            assert_refused_naming(
                &format!("[brigade]\nworker_agent = {bad}\n"),
                "worker_agent",
                bad.trim_matches('"'),
                &["inherit", "claude", "codex", "select"],
            );
        }
        for (ok, setting) in [
            ("inherit", WorkerAgentSetting::Inherit),
            ("select", WorkerAgentSetting::Select),
            (" Claude ", WorkerAgentSetting::Claude),
            ("CODEX", WorkerAgentSetting::Codex),
        ] {
            let config = parse(&format!("[brigade]\nworker_agent = \"{ok}\"\n"));
            assert_eq!(config.brigade.worker_agent, setting, "{ok}");
        }
    }

    #[test]
    fn goinkyo_consult_refuses_a_value_that_is_not_a_level_or_empty() {
        for bad in ["\"decision\"", "\"typo\"", "\"on\""] {
            assert_refused_naming(
                &format!("[brigade]\ngoinkyo_consult = {bad}\n"),
                "goinkyo_consult",
                bad.trim_matches('"'),
                &["off", "disputes", "design", "decisions"],
            );
        }
        // Not a string at all: the same as any wrong type.
        for bad in ["false", "3", "1.5", "[\"off\"]", "{ level = \"off\" }"] {
            assert_wrong_type(&format!("[brigade]\ngoinkyo_consult = {bad}\n"), "a string");
        }
        for ok in [
            "off",
            "disputes",
            "design",
            "decisions",
            " DESIGN ",
            "",
            "   ",
        ] {
            let config = parse(&format!("[brigade]\ngoinkyo_consult = \"{ok}\"\n"));
            assert_eq!(config.brigade.goinkyo_consult, ok);
        }
    }

    #[test]
    fn worker_autocompact_refuses_what_neither_product_takes_and_keeps_what_is_documented() {
        for bad in [
            "99999",
            "1000001",
            "-5",
            "\"abc\"",
            "\"250k\"",
            "\"1e6\"",
            "\"+250000\"",
            "\"99999\"",
        ] {
            assert_refused_naming(
                &format!("[brigade]\nworker_autocompact = {bad}\n"),
                "worker_autocompact",
                bad.trim_matches('"'),
                &["auto", "100000", "1000000"],
            );
        }
        for bad in ["true", "1.5", "250000.0", "[250000]", "{ n = 1 }"] {
            assert_wrong_type(
                &format!("[brigade]\nworker_autocompact = {bad}\n"),
                "an integer or a string",
            );
        }
        for (ok, passes) in [
            ("250000", Some("250000")),
            ("100000", Some("100000")),
            ("1000000", Some("1000000")),
            ("\"300000\"", Some("300000")),
            ("\"auto\"", Some("auto")),
            ("\"AUTO\"", Some("auto")),
            ("\" 300000 \"", Some("300000")),
            ("\"\"", None),
        ] {
            let config = parse(&format!("[brigade]\nworker_autocompact = {ok}\n"));
            assert_eq!(
                config.brigade.worker_autocompact().as_deref(),
                passes,
                "{ok}"
            );
        }
    }

    #[test]
    fn agents_refuses_a_name_that_is_not_a_product_even_beside_one_that_is() {
        for bad in [
            "\"made-up\"",
            "\"claude,made-up\"",
            "\"claude,\"",
            "\"claude codex\"",
        ] {
            assert_refused_naming(
                &format!("agents = {bad}\n"),
                "agents",
                bad.trim_matches('"'),
                &["all", "claude", "codex"],
            );
        }
        for ok in [
            "",
            "all",
            "ALL",
            "claude",
            "codex",
            "claude,codex",
            " Claude , CODEX ",
        ] {
            assert_eq!(parse(&format!("agents = \"{ok}\"\n")).agents, ok);
        }
    }

    #[test]
    fn keys_prefix_refuses_what_is_not_a_chord() {
        for bad in ["\"\"", "\"C-\"", "\"ctrl-b\"", "\"ab\"", "\"C-bb\""] {
            assert_refused_naming(
                &format!("[keys]\nprefix = {bad}\n"),
                "prefix",
                bad.trim_matches('"'),
                &["C-"],
            );
        }
        for ok in ["C-b", "C-]", "`", " ", "x"] {
            assert_eq!(
                parse(&format!("[keys]\nprefix = \"{ok}\"\n")).keys.prefix,
                ok
            );
        }
    }

    #[test]
    fn unknown_keys_and_free_text_settings_still_load() {
        let config = parse(
            "no_such_key = 1\n[no_such_table]\nx = 1\n[brigade]\nno_such = true\n\
             worker_model = \"whatever-model\"\nworker_effort = \"extreme\"\n\
             goinkyo_permission_mode = \"anything\"\nworker_model_codex = \"\"\n",
        );
        assert_eq!(config.brigade.worker_model, "whatever-model");
        assert_eq!(config.brigade.worker_effort, "extreme");
        assert_eq!(config.brigade.worker_model_codex, "");
    }

    // -- product tables: [brigade.claude] and [brigade.codex] ----------------

    /// Every setting the two product tables hold, read through its accessor, as
    /// text: an empty string is "no flag".
    fn product_settings(brigade: &BrigadeConfig) -> Vec<String> {
        let own = |value: Option<&str>| value.unwrap_or_default().to_string();
        vec![
            own(brigade.worker_model_for(AgentKind::ClaudeCode)),
            own(brigade.worker_effort_for(AgentKind::ClaudeCode)),
            own(brigade.worker_permission_mode()),
            own(brigade.goinkyo_model()),
            own(brigade.goinkyo_effort()),
            own(brigade.goinkyo_permission_mode()),
            own(brigade.goinkyo_disallowed_tools()),
            own(brigade.worker_model_for(AgentKind::Codex)),
            own(brigade.worker_effort_for(AgentKind::Codex)),
        ]
    }

    /// The order of [`product_settings`]: claude worker model, effort and
    /// permission mode; claude goinkyo model, effort, permission mode and
    /// disallowed tools; codex worker model and effort.
    const LEGACY_ONLY: &str = "[brigade]\n\
        worker_model = \"l-wm\"\nworker_effort = \"l-we\"\nworker_permission_mode = \"l-wp\"\n\
        goinkyo_model = \"l-gm\"\ngoinkyo_effort = \"l-ge\"\ngoinkyo_permission_mode = \"l-gp\"\n\
        goinkyo_disallowed_tools = \"l-gd\"\n\
        worker_model_codex = \"l-cm\"\nworker_effort_codex = \"l-ce\"\n";

    const TABLES_ONLY: &str = "[brigade.claude]\n\
        worker_model = \"t-wm\"\nworker_effort = \"t-we\"\nworker_permission_mode = \"t-wp\"\n\
        goinkyo_model = \"t-gm\"\ngoinkyo_effort = \"t-ge\"\ngoinkyo_permission_mode = \"t-gp\"\n\
        goinkyo_disallowed_tools = \"t-gd\"\n\
        [brigade.codex]\nworker_model = \"t-cm\"\nworker_effort = \"t-ce\"\n";

    #[test]
    fn a_file_with_only_the_legacy_keys_reads_as_it_always_did() {
        let config = parse(LEGACY_ONLY);
        assert_eq!(
            product_settings(&config.brigade),
            [
                "l-wm", "l-we", "l-wp", "l-gm", "l-ge", "l-gp", "l-gd", "l-cm", "l-ce"
            ]
        );
        assert_eq!(config.brigade.claude, ClaudeBrigadeConfig::default());
        assert_eq!(config.brigade.codex, CodexBrigadeConfig::default());
    }

    #[test]
    fn a_file_with_only_the_product_tables_reads_them() {
        let config = parse(TABLES_ONLY);
        assert_eq!(
            product_settings(&config.brigade),
            [
                "t-wm", "t-we", "t-wp", "t-gm", "t-ge", "t-gp", "t-gd", "t-cm", "t-ce"
            ]
        );
    }

    #[test]
    fn a_file_with_neither_keeps_every_built_in() {
        let built_in = [
            "sonnet",
            "high",
            "auto",
            "opus",
            "xhigh",
            "auto",
            "Edit,Write,NotebookEdit",
            "terra",
            "high",
        ];
        assert_eq!(product_settings(&BrigadeConfig::default()), built_in);
        assert_eq!(
            product_settings(&parse("[brigade]\nworkers = 2\n").brigade),
            built_in
        );
    }

    #[test]
    fn the_table_wins_over_the_legacy_key_one_setting_at_a_time() {
        // All nine in both places: every setting is the table's.
        let both = parse(&format!("{LEGACY_ONLY}{TABLES_ONLY}"));
        assert_eq!(
            product_settings(&both.brigade),
            [
                "t-wm", "t-we", "t-wp", "t-gm", "t-ge", "t-gp", "t-gd", "t-cm", "t-ce"
            ]
        );
        // Only some in the table: the rest still come from their legacy key, then
        // from the built-in, so a half-migrated file means what it says.
        let mixed = parse(
            "[brigade]\nworker_model = \"l-wm\"\nworker_effort = \"l-we\"\n\
             worker_model_codex = \"l-cm\"\n\
             [brigade.claude]\nworker_model = \"t-wm\"\n",
        );
        assert_eq!(
            product_settings(&mixed.brigade),
            [
                "t-wm",
                "l-we",
                "auto",
                "opus",
                "xhigh",
                "auto",
                "Edit,Write,NotebookEdit",
                "l-cm",
                "high"
            ]
        );
    }

    #[test]
    fn a_table_value_set_to_empty_is_the_escape_hatch_whatever_the_legacy_key_says() {
        let config = parse(
            "[brigade]\nworker_model = \"l-wm\"\nworker_model_codex = \"l-cm\"\n\
             goinkyo_disallowed_tools = \"l-gd\"\n\
             [brigade.claude]\nworker_model = \"\"\ngoinkyo_disallowed_tools = \"\"\n\
             [brigade.codex]\nworker_model = \"  \"\n",
        );
        assert_eq!(config.brigade.worker_model_for(AgentKind::ClaudeCode), None);
        assert_eq!(config.brigade.worker_model_for(AgentKind::Codex), None);
        assert_eq!(config.brigade.goinkyo_disallowed_tools(), None);
        // And an empty legacy key under a table that sets a value changes nothing.
        let table_over_empty =
            parse("[brigade]\nworker_model = \"\"\n[brigade.claude]\nworker_model = \"t-wm\"\n");
        assert_eq!(
            table_over_empty
                .brigade
                .worker_model_for(AgentKind::ClaudeCode),
            Some("t-wm")
        );
    }

    #[test]
    fn a_product_table_banto_does_not_know_is_ignored_like_any_unknown_key() {
        let config = parse(
            "[brigade]\nworker_model = \"l-wm\"\n\
             [brigade.antigravity]\nworker_model = \"x\"\nanything = 5\n\
             [brigade.claude]\nno_such_key = true\n\
             [brigade.codex]\nno_such_key = \"nothing reads this\"\n",
        );
        assert_eq!(
            config.brigade.worker_model_for(AgentKind::ClaudeCode),
            Some("l-wm")
        );
        // A key a known table does not have is ignored too.
        assert_eq!(config.brigade.codex, CodexBrigadeConfig::default());
        assert_eq!(config.brigade.claude, ClaudeBrigadeConfig::default());
    }

    #[test]
    fn a_codex_goinkyo_has_its_own_model_and_effort_and_no_built_in() {
        use AgentKind::{ClaudeCode, Codex};
        let built_in = BrigadeConfig::default();
        assert_eq!(built_in.goinkyo_model_for(Codex), None);
        assert_eq!(built_in.goinkyo_effort_for(Codex), None);
        // The Claude Goinkyo's built-ins are what they were.
        assert_eq!(built_in.goinkyo_model_for(ClaudeCode), Some("opus"));
        assert_eq!(built_in.goinkyo_effort_for(ClaudeCode), Some("xhigh"));

        let config = parse(
            "[brigade.codex]\ngoinkyo_model = \" terra \"\ngoinkyo_effort = \"xhigh\"\n\
             [brigade.claude]\ngoinkyo_model = \"fable\"\ngoinkyo_effort = \"max\"\n",
        );
        let brigade = &config.brigade;
        assert_eq!(brigade.goinkyo_model_for(Codex), Some("terra"));
        assert_eq!(brigade.goinkyo_effort_for(Codex), Some("xhigh"));
        // Each product reads its own table and never the other's.
        assert_eq!(brigade.goinkyo_model_for(ClaudeCode), Some("fable"));
        assert_eq!(brigade.goinkyo_effort_for(ClaudeCode), Some("max"));
        assert_eq!(brigade.goinkyo_model(), Some("fable"));
        assert_eq!(brigade.goinkyo_effort(), Some("max"));
        // A Codex Worker's keys are its own: still the built-in.
        assert_eq!(
            brigade.worker_model_for(Codex),
            BrigadeConfig::default().worker_model_for(Codex)
        );
    }

    #[test]
    fn a_codex_goinkyo_key_has_no_legacy_spelling_and_the_flat_goinkyo_keys_stay_claudes() {
        use AgentKind::{ClaudeCode, Codex};
        let config = parse(
            "[brigade]\ngoinkyo_model = \"l-gm\"\ngoinkyo_effort = \"low\"\n\
             goinkyo_model_codex = \"nope\"\ngoinkyo_effort_codex = \"nope\"\n",
        );
        let brigade = &config.brigade;
        assert_eq!(brigade.goinkyo_model_for(ClaudeCode), Some("l-gm"));
        assert_eq!(brigade.goinkyo_effort_for(ClaudeCode), Some("low"));
        assert_eq!(brigade.goinkyo_model_for(Codex), None);
        assert_eq!(brigade.goinkyo_effort_for(Codex), None);
        assert!(brigade.codex.goinkyo_model.is_none() && brigade.codex.goinkyo_effort.is_none());
    }

    #[test]
    fn an_empty_codex_goinkyo_value_passes_nothing_and_is_not_validated() {
        use AgentKind::Codex;
        let empty = parse("[brigade.codex]\ngoinkyo_model = \"\"\ngoinkyo_effort = \"  \"\n");
        assert_eq!(empty.brigade.goinkyo_model_for(Codex), None);
        assert_eq!(empty.brigade.goinkyo_effort_for(Codex), None);
        let odd = parse(
            "[brigade.codex]\ngoinkyo_model = \"gpt-nonsense\"\ngoinkyo_effort = \"extreme\"\n",
        );
        assert_eq!(odd.brigade.goinkyo_model_for(Codex), Some("gpt-nonsense"));
        assert_eq!(odd.brigade.goinkyo_effort_for(Codex), Some("extreme"));
        assert_wrong_type("[brigade.codex]\ngoinkyo_model = 5\n", "a string");
        assert_wrong_type("[brigade.codex]\ngoinkyo_effort = true\n", "a string");
    }

    // --- a [brigade] key written below a [brigade.*] header ---------------------

    /// `text` is refused for `key` being misplaced under `[brigade.<table>]`, at
    /// `line`, with the offending line echoed as `toml` does for a wrong type.
    fn assert_misplaced(text: &str, line: usize, key: &str, table: &str) -> String {
        let message = refusal(text).unwrap_or_else(|| panic!("{text:?} loaded"));
        assert!(
            message.starts_with(&format!("TOML parse error at line {line}, column ")),
            "{message}"
        );
        assert!(
            message.contains(&format!("`{key}` "))
                && message.contains(&format!("[brigade.{table}]")),
            "{message}"
        );
        message
    }

    #[test]
    fn a_plain_brigade_key_under_a_product_table_is_refused_where_it_is() {
        // Not refused as a typo and not read at all, before this: the key
        // belongs to the table above it.
        let message = assert_misplaced(
            "[brigade.claude]\nworker_model = \"x\"\nrelay = \"typo\"\n",
            3,
            "relay",
            "claude",
        );
        assert!(
            message.contains(
                "`relay` is a [brigade] key, but it is written under [brigade.claude]; \
                 move it above the first [brigade.*] table"
            ),
            "{message}"
        );
        // The offending line is shown, which is what makes the line number useful.
        assert!(message.contains("relay = \"typo\""), "{message}");
        assert_misplaced(
            "[brigade.codex]\nworker_effort = \"low\"\nworkers = 5\n",
            3,
            "workers",
            "codex",
        );
        // Every plain key, under either table, however good its value is.
        for table in ["claude", "codex"] {
            for key in PLAIN_BRIGADE_KEYS {
                let text = format!("[brigade.{table}]\n{key} = 1\n");
                assert_misplaced(&text, 2, key, table);
            }
        }
    }

    #[test]
    fn a_legacy_flat_key_the_table_does_not_have_is_refused_where_it_is() {
        let message = assert_misplaced(
            "[brigade.claude]\nworker_model = \"x\"\nworker_model_codex = \"terra\"\n",
            3,
            "worker_model_codex",
            "codex",
        );
        assert!(
            message.contains(
                "`worker_model_codex` is a legacy [brigade] key for `worker_model` under \
                 [brigade.codex], but it is written under [brigade.claude]"
            ),
            "{message}"
        );
        // The same key in its own table: the name has changed there.
        let message = assert_misplaced(
            "[brigade.codex]\nworker_effort_codex = \"low\"\n",
            2,
            "worker_effort_codex",
            "codex",
        );
        assert!(
            message.contains("is the old [brigade] spelling of `worker_effort`"),
            "{message}"
        );
        // A Claude-only legacy name under the Codex table.
        assert_misplaced(
            "[brigade.codex]\ngoinkyo_permission_mode = \"auto\"\n",
            2,
            "goinkyo_permission_mode",
            "claude",
        );
        assert_misplaced(
            "[brigade.codex]\ngoinkyo_disallowed_tools = \"\"\n",
            2,
            "goinkyo_disallowed_tools",
            "claude",
        );
        assert_misplaced(
            "[brigade.claude]\nworker_effort_codex = \"low\"\n",
            2,
            "worker_effort_codex",
            "codex",
        );
    }

    #[test]
    fn a_key_that_is_the_tables_own_or_that_nothing_knows_still_loads() {
        // Each table's own keys, including the ones the other table has too, and
        // the shared name (`worker_model`) that is a legacy flat key as well.
        for key in CLAUDE_TABLE_KEYS {
            assert_eq!(misplaced_key("claude", &CLAUDE_TABLE_KEYS, key), None);
        }
        for key in CODEX_TABLE_KEYS {
            assert_eq!(misplaced_key("codex", &CODEX_TABLE_KEYS, key), None);
        }
        // Unknown keys stay ignored, a typo of a plain key among them.
        let config = parse(
            "[brigade.claude]\nrelay_ = \"x\"\nsome_future_key = 1\n\
             [brigade.codex]\nWorkers = 5\nnothing = true\n",
        );
        assert_eq!(config.brigade.claude, ClaudeBrigadeConfig::default());
        assert_eq!(config.brigade.codex, CodexBrigadeConfig::default());
        // And the plain keys above the first table are read, as they always were.
        let config = parse(
            "[brigade]\nworkers = 5\nrelay = \"manual\"\n[brigade.claude]\nworker_model = \"x\"\n",
        );
        assert_eq!(config.brigade.workers, 5);
        assert_eq!(config.brigade.relay, RelayMode::Manual);
    }

    #[test]
    fn the_key_lists_name_every_field_their_struct_has() {
        // A field added to a struct breaks the destructuring below, which is the
        // place to add it to the list: a misplaced key not on the list is not
        // refused. The list is then held to the real field names by setting
        // every key of it and finding each one set.
        let ClaudeBrigadeConfig {
            worker_model,
            worker_effort,
            worker_permission_mode,
            goinkyo_model,
            goinkyo_effort,
            goinkyo_permission_mode,
            goinkyo_disallowed_tools,
        } = parse(&format!(
            "[brigade.claude]\n{}",
            CLAUDE_TABLE_KEYS
                .map(|key| format!("{key} = \"x\"\n"))
                .concat()
        ))
        .brigade
        .claude;
        for field in [
            worker_model,
            worker_effort,
            worker_permission_mode,
            goinkyo_model,
            goinkyo_effort,
            goinkyo_permission_mode,
            goinkyo_disallowed_tools,
        ] {
            assert_eq!(field.as_deref(), Some("x"));
        }
        let CodexBrigadeConfig {
            worker_model,
            worker_effort,
            goinkyo_model,
            goinkyo_effort,
        } = parse(&format!(
            "[brigade.codex]\n{}",
            CODEX_TABLE_KEYS
                .map(|key| format!("{key} = \"x\"\n"))
                .concat()
        ))
        .brigade
        .codex;
        for field in [worker_model, worker_effort, goinkyo_model, goinkyo_effort] {
            assert_eq!(field.as_deref(), Some("x"));
        }
        // The plain keys: one by one, each one set to something other than its
        // built-in changes the parsed config, so each is a real field.
        let built_in = BrigadeConfig::default();
        for (key, value) in [
            ("workers", "7"),
            ("worker_agent", "\"codex\""),
            ("worker_autocompact", "\"auto\""),
            ("relay", "\"manual\""),
            ("request_attention", "false"),
            ("director_prompt", "\"d\""),
            ("goinkyo_consult", "\"off\""),
            ("worker_prompt", "\"w\""),
            ("goinkyo_prompt", "\"g\""),
            ("goinkyo_agent", "\"codex\""),
            ("message_log", "true"),
        ] {
            assert!(PLAIN_BRIGADE_KEYS.contains(&key), "{key}");
            let parsed = parse(&format!("[brigade]\n{key} = {value}\n")).brigade;
            assert_ne!(parsed, built_in, "{key} is not a field of BrigadeConfig");
        }
        let BrigadeConfig {
            workers: _,
            claude: _,
            codex: _,
            worker_agent: _,
            worker_autocompact: _,
            relay: _,
            request_attention: _,
            director_prompt: _,
            goinkyo_consult: _,
            worker_prompt: _,
            goinkyo_prompt: _,
            goinkyo_agent: _,
            message_log: _,
            worker_model: _,
            worker_effort: _,
            worker_permission_mode: _,
            goinkyo_model: _,
            goinkyo_effort: _,
            goinkyo_permission_mode: _,
            goinkyo_disallowed_tools: _,
            worker_model_codex: _,
            worker_effort_codex: _,
            legacy_keys: _,
        } = built_in;
        // 23 fields: the 11 plain keys, the two tables, the 9 legacy keys and
        // `legacy_keys`.
        assert_eq!(
            PLAIN_BRIGADE_KEYS.len() + 2 + LEGACY_BRIGADE_KEYS.len() + 1,
            23
        );
    }

    #[test]
    fn a_value_of_the_wrong_type_is_refused_in_a_table_and_under_a_legacy_key_alike() {
        assert_wrong_type("[brigade.claude]\nworker_model = 5\n", "a string");
        assert_wrong_type("[brigade.codex]\nworker_effort = true\n", "a string");
        assert_wrong_type("[brigade]\nworker_model = 5\n", "a string");
        assert_wrong_type("[brigade]\nworker_effort_codex = [\"high\"]\n", "a string");
        // A legacy key beside a good table is still checked.
        assert_wrong_type(
            "[brigade]\ngoinkyo_model = 5\n[brigade.claude]\ngoinkyo_model = \"opus\"\n",
            "a string",
        );
        // And the table has to be a table.
        assert!(refusal("[brigade]\nclaude = \"opus\"\n").is_some());
    }

    #[test]
    fn values_that_go_to_an_agent_are_not_validated_in_a_table_either() {
        let config = parse(
            "[brigade.claude]\nworker_effort = \"extreme\"\nworker_permission_mode = \"anything\"\n\
             [brigade.codex]\nworker_model = \"gpt-nonsense\"\n",
        );
        assert_eq!(
            config.brigade.worker_effort_for(AgentKind::ClaudeCode),
            Some("extreme")
        );
        assert_eq!(config.brigade.worker_permission_mode(), Some("anything"));
        assert_eq!(
            config.brigade.worker_model_for(AgentKind::Codex),
            Some("gpt-nonsense")
        );
    }

    #[test]
    fn the_legacy_keys_are_found_by_name_in_the_order_the_notice_gives() {
        assert_eq!(
            legacy_brigade_keys_in([
                "worker_effort_codex",
                "relay",
                "claude",
                "goinkyo_model",
                "worker_model",
                "worker_model_codex_",
            ]),
            ["worker_model", "goinkyo_model", "worker_effort_codex"]
        );
        assert!(legacy_brigade_keys_in(["relay", "workers", "claude", "codex"]).is_empty());
    }

    #[test]
    fn the_legacy_key_notice_names_each_key_and_its_new_home() {
        let brigade = BrigadeConfig {
            legacy_keys: vec![
                "worker_model",
                "goinkyo_disallowed_tools",
                "worker_model_codex",
            ],
            ..BrigadeConfig::default()
        };
        assert_eq!(
            brigade.legacy_key_notice().as_deref(),
            Some(
                "config: legacy [brigade] keys (worker_model, goinkyo_disallowed_tools, \
                 worker_model_codex) moved to [brigade.claude]/[brigade.codex]; see README"
            )
        );
    }

    #[test]
    fn the_legacy_key_notice_stays_short_however_many_keys_the_file_has() {
        // The status line clips what it draws at the terminal's width: nine keys
        // said in full were ~690 characters, one key ~96.
        let all = BrigadeConfig {
            legacy_keys: LEGACY_BRIGADE_KEYS.iter().map(|key| key.legacy).collect(),
            ..BrigadeConfig::default()
        };
        let notice = all.legacy_key_notice().unwrap();
        assert_eq!(
            notice,
            "config: legacy [brigade] keys (worker_model, worker_effort, \
             worker_permission_mode and 6 more) moved to [brigade.claude]/[brigade.codex]; \
             see README"
        );
        assert!(notice.chars().count() < 150, "{}", notice.chars().count());
        let one = BrigadeConfig {
            legacy_keys: vec!["worker_model"],
            ..BrigadeConfig::default()
        };
        assert!(one.legacy_key_notice().unwrap().chars().count() < 110);
    }

    #[test]
    fn the_legacy_key_notice_says_a_legacy_key_is_ignored_when_its_table_sets_it() {
        let brigade = BrigadeConfig {
            legacy_keys: vec!["worker_effort", "worker_effort_codex"],
            claude: ClaudeBrigadeConfig {
                worker_effort: Some("low".to_string()),
                ..ClaudeBrigadeConfig::default()
            },
            ..BrigadeConfig::default()
        };
        assert_eq!(
            brigade.legacy_key_notice().as_deref(),
            Some(
                "config: legacy [brigade] keys (worker_effort, worker_effort_codex) moved to \
                 [brigade.claude]/[brigade.codex]; see README; 1 ignored (the table wins)"
            )
        );
    }

    #[test]
    fn there_is_no_legacy_key_notice_when_the_file_wrote_none() {
        assert_eq!(BrigadeConfig::default().legacy_key_notice(), None);
        // A product table alone is the new spelling, not a legacy one.
        assert_eq!(parse(TABLES_ONLY).brigade.legacy_key_notice(), None);
    }
}
