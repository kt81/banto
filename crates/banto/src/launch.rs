//! How an agent is started: the argv for each product ([`AgentLaunch`]), the
//! `-c` overrides that carry a brigade member's identity into a Codex session
//! ([`CodexBrigade`]), the binary an `[agent_binaries]` entry selects
//! ([`agent_binary`]), and the check that refuses to resume a session that is
//! already running somewhere ([`is_live`]) — AGENTS.md invariant 4.
//!
//! The emporium (`crate::embedded::emporium`) is the only caller. Liveness is
//! judged purely by PID, the way `banto_io::status` judges session activity.

use std::path::{Path, PathBuf};

use banto_core::config::AgentBinaries;
use banto_core::model::{AgentKind, BrigadeRole, SessionToOpen};
use banto_io::codex_home::CodexHome;
use banto_io::codex_liveness::{self, ProcessStartTime};
use banto_io::status::{LiveSession, ProcessProbe};

/// Read-only inputs an open/resume decision needs beyond the session itself
/// — bundled once `open_session`'s own argument list hit clippy's
/// too-many-arguments limit, rather than letting it keep growing one field
/// at a time.
pub struct OpenContext<'a> {
    pub probe: &'a dyn ProcessProbe,
    /// The current `<claude_home>/sessions/*.json` snapshot (see
    /// `banto_io::status::read_live_sessions`). Claude sessions only —
    /// Codex liveness goes through `codex_home`/`start_time` instead (see
    /// [`is_live`]).
    pub live: &'a [LiveSession],
    /// `None` degrades a Codex session to "not live" — see [`is_live`].
    pub codex_home: Option<&'a CodexHome>,
    pub start_time: &'a dyn ProcessStartTime,
}

/// Whether `session` is currently running somewhere — in a pane of this banto
/// or another, or in an agent started by hand. The emporium refuses to resume
/// one that is: a second `--resume` would fork its history (AGENTS.md
/// invariant 4).
///
/// `session.agent` picks the source: Claude Code checks `ctx.live` (any
/// entry naming `session.id` with a currently-alive pid, regardless of
/// busy/idle status — unlike `banto_io::status::classify`, which is about
/// which activity dot to show, this only answers "is this actually running
/// right now"); Codex checks `ctx.codex_home` via
/// `codex_liveness::is_thread_alive`, which degrades to `false` — not live
/// — whenever it can't be determined (see that function's doc for why
/// under-reporting is the safe direction here too).
///
/// For Claude, when `entry` carries a `proc_start` (Claude Code's own
/// record of its process's kernel start time), liveness is checked via
/// [`ProcessProbe::is_alive_matching`] instead of a bare
/// [`ProcessProbe::is_alive`] — so a pid recycled by an unrelated process
/// since the live-state file was written is not mistaken for the original
/// session still running, which would otherwise block a resume forever with
/// no self-heal (see that method's doc comment).
///
/// This intentionally remains a fresh point query, rather than using the
/// list's batched liveness snapshot: opening/resuming is a user action and
/// the `proc_start` identity comparison is a correctness guard, not a
/// reload-path throughput question.
pub(crate) fn is_live(session: &SessionToOpen, ctx: &OpenContext) -> bool {
    match session.agent {
        AgentKind::ClaudeCode => ctx.live.iter().any(|entry| {
            entry.session_id.as_deref() == Some(session.id.as_str())
                && match &entry.proc_start {
                    Some(proc_start) => ctx.probe.is_alive_matching(entry.pid, proc_start),
                    None => ctx.probe.is_alive(entry.pid),
                }
        }),
        AgentKind::Codex => ctx.codex_home.is_some_and(|codex_home| {
            codex_liveness::is_thread_alive(codex_home, &session.id, ctx.start_time)
        }),
    }
}

/// A pending agent invocation, turned into argv by [`Self::argv`]. This is the
/// single place in the workspace that knows which flags each agent product
/// takes and in what order.
///
/// One variant per [`AgentKind`], not one struct with optional fields
/// spanning every product: `append_system_prompt`/`mcp_config` are Claude
/// Code concepts with no Codex equivalent (Codex 0.145's own `--help` has
/// no `--append-system-prompt`; its MCP config goes through
/// `-c mcp_servers.…` instead — see [`CodexBrigade`] for how a Codex launch
/// carries the same brigade identity by a different mechanism). A struct
/// with both products' fields left `None` for whichever one doesn't apply
/// would rely on every caller remembering to leave them unset; an enum
/// makes a launch physically unable to carry the other product's fields,
/// whether or not a caller remembers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentLaunch {
    Claude {
        resume: Option<String>,
        model: Option<String>,
        /// `--effort <level>` (`claude --help`: `low`/`medium`/`high`/
        /// `xhigh`/`max`) — so far only ever set for an auto-spawned
        /// Goinkyo (`BrigadeConfig::goinkyo_effort`). No Codex counterpart
        /// on [`Self::Codex`]: Claude's reasoning-effort flag has no
        /// equivalent there (Codex's nearest is `-c
        /// model_reasoning_effort`, a `-c` override like the rest of
        /// [`CodexBrigade`], not a bare flag) — giving `Self::Codex` a
        /// field this build never fills would be exactly the "unused field
        /// nobody remembers to leave `None`" risk this enum's own doc says
        /// the per-variant split exists to avoid.
        effort: Option<String>,
        /// `--permission-mode <mode>` (`claude --help`: `manual`/
        /// `acceptEdits`/`plan`/`auto`/`dontAsk`/`bypassPermissions`) —
        /// unlike `effort` above, set for a Goinkyo
        /// (`BrigadeConfig::goinkyo_permission_mode`) on a resume too, not
        /// just a fresh spawn: who may approve a tool call is a property of
        /// the role, not of whether this particular launch is fresh. Same
        /// "no Codex counterpart on `Self::Codex`" reasoning as `effort`.
        permission_mode: Option<String>,
        /// `--disallowedTools <tools>` (`claude --help`: comma- or
        /// space-separated tool names to deny) —
        /// `BrigadeConfig::goinkyo_disallowed_tools`, same scope as
        /// `permission_mode` just above (Goinkyo, fresh spawn or resume
        /// alike — which tools exist to call is as much a role property as
        /// who approves them) and the same "no Codex counterpart" reasoning
        /// as `effort`.
        disallowed_tools: Option<String>,
        append_system_prompt: Option<String>,
        mcp_config: Option<PathBuf>,
    },
    /// Codex takes its working directory as `-C <dir>` in argv itself
    /// (measured against Codex 0.145: unlike Claude, which only ever gets a
    /// cwd via the spawner's own `current_dir`/`Command::current_dir`) — so
    /// `cwd` lives on the launch here, even though [`Self::Claude`]'s own
    /// rendering keeps ignoring it in favor of the spawner.
    Codex {
        resume: Option<String>,
        model: Option<String>,
        cwd: PathBuf,
        /// `None` outside brigade context (an ordinary resume/new session);
        /// `Some` adds the `-c` overrides that make this launch a brigade
        /// member — see [`CodexBrigade`].
        brigade: Option<CodexBrigade>,
    },
}

/// Brigade identity for a Codex launch, carried entirely as `-c` overrides
/// instead of `~/.codex/config.toml` — measured 2026-07-28
/// (`docs/notes/codex-briefing-spike.md`): a `-c`-registered MCP server
/// can't persist trust, and its tool-approval bypass degrades silently
/// (not an error) the moment a launch omits it, so both are supplied fresh
/// on every launch rather than written once to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexBrigade {
    /// banto's own executable, re-invoked both as the `SessionStart` hook's
    /// command and as banto's own MCP server — see
    /// [`session_start_hook_override`]'s doc for why this is the *only*
    /// field of this struct the hook override may ever depend on.
    pub(crate) exe: PathBuf,
    pub(crate) brigade_id: i64,
    pub(crate) token: String,
    pub(crate) role: BrigadeRole,
    /// This member's own session id, once discovered — `None` for a
    /// freshly-spawned Worker still awaiting one (mirrors
    /// `embedded::emporium::write_mcp_config`'s `--session` fallback).
    pub(crate) session: Option<String>,
    /// What the parent banto resolved (`startup::Resolved::child_args`), so the
    /// MCP server it starts reads the same config, database and homes rather
    /// than finding its own.
    pub(crate) child_args: Vec<String>,
}

/// Resolve the binary to invoke for `agent`: the operator's own
/// `[agent_binaries]` override from config.toml, or the product's own bare
/// name (found via `$PATH`) when unset — today's behavior, preserved as the
/// default for both products.
pub(crate) fn agent_binary(agent: AgentKind, binaries: &AgentBinaries) -> String {
    let (override_path, default_name) = match agent {
        AgentKind::ClaudeCode => (&binaries.claude, "claude"),
        AgentKind::Codex => (&binaries.codex, "codex"),
    };
    match override_path {
        Some(path) => path.to_string_lossy().into_owned(),
        None => default_name.to_string(),
    }
}

/// Escapes `\` and `"` for embedding in the body of a TOML basic string —
/// the content between the quotes, not the quotes themselves, which every
/// call site below still writes explicitly. Order matters — backslashes
/// first, then quotes — or a backslash this function just inserted to
/// escape a `"` would itself get escaped a second time by a later pass.
///
/// [`forward_slash_path`] already eliminates backslashes from the one value
/// here that would otherwise be full of them (a Windows path), so both
/// passes below are defense against a byte class none of banto's own
/// current inputs (a forward-slash path, an `_mcp` argument) are known to
/// contain, not a load-bearing part of any of them. Letting a stray `\` or
/// `"` through unescaped would silently corrupt the outer TOML
/// `command="..."`/`args=[...]` value it sits inside rather than error, so
/// it stays in as cheap insurance.
fn toml_basic_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Normalizes a path to forward slashes for embedding in a `-c` override.
/// Codex's own `-c` value parser accepts both separators on Windows
/// (measured 2026-07-28, docs/notes/codex-briefing-spike.md) — forward
/// slashes are used unconditionally rather than backslashes-plus-escaping
/// because the double-escaping trap is easy to get wrong by hand (measured
/// the same day: a one-off manual test slipped to *four* backslashes and
/// happened to still work only because Windows itself tolerates the extra
/// pair — a mistake structurally impossible once there's nothing left to
/// double-escape).
pub(crate) fn forward_slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Whether `exe` (already forward-slash-normalized) can be embedded bare as
/// the hook command's first token. Codex splits `command` on whitespace
/// itself before spawning it, and a space anywhere in that first token
/// breaks the split — quoting was tried as the fix (worker-3, measured
/// 2026-07-29, `docs/notes/codex-briefing-spike.md`) and made the hook fail
/// *unconditionally*, quoted or not, space or no space: there is no working
/// quoting scheme for token 0. Every caller of
/// [`session_start_hook_override`] must check this first and, when it
/// returns `false`, skip the hook `-c` entirely — emitting a command that
/// can never fire trades a working briefing for a bare "SessionStart
/// Failed" in the pane and nothing more useful than that.
pub(crate) fn hook_command_is_launchable(exe: &str) -> bool {
    !exe.contains(' ')
}

/// The `-c hooks.SessionStart=...` override that makes banto's own
/// briefing hook fire. Depends on nothing but `exe` — no brigade id, member
/// token, role, or session id — because Codex keys hook trust off a hash of
/// this exact command string (measured 2026-07-28): one differing byte
/// between two members, or between two launches of the same member, drops
/// that member back to untrusted, and an untrusted hook is skipped with no
/// warning at all — not an error, not a log line. `matcher`/`timeout_sec`
/// are spelled out explicitly rather than left to Codex's own defaults for
/// the same reason: a future default change would shift the hash under us
/// with no visible cause either.
///
/// `exe` is embedded bare, with no quoting at all, unlike an earlier
/// version of this function that wrapped it in literal `"` characters to
/// protect a path containing a space: that was measured (worker-3,
/// 2026-07-29) to break the hook regardless of the space, quoted or not —
/// there is no working fix here, only [`hook_command_is_launchable`]
/// deciding up front whether to call this function at all. `_hook` is a
/// fixed literal with no spaces, so it was never the problem.
///
/// `pub(crate)` specifically so [`crate::codex_trust`] can call this same
/// function rather than reconstruct its own copy of the string: the whole
/// point of `codex-trust` is to show the operator, in a standalone `codex`
/// session with nothing else running, the *exact* hook they're about to
/// trust everywhere else. A hand-copied second implementation would drift
/// the moment either one changed, and the two need not even land in the
/// same commit for that to happen quietly.
pub(crate) fn session_start_hook_override(exe: &str) -> String {
    let command = toml_basic_string(&format!("{exe} _hook"));
    format!(
        "hooks.SessionStart=[{{matcher=\"\",hooks=[{{type=\"command\",timeout_sec=600,command=\"{command}\"}}]}}]"
    )
}

/// `-c mcp_servers.banto.command=...`: banto re-invoked as its own MCP
/// server. Same `exe` as [`session_start_hook_override`], but *not*
/// quote-wrapped: `command`/`args` here are structured TOML fields Codex
/// hands to its own process spawner directly, never re-split by a shell the
/// way the hook's packed command string is (measured 2026-07-28) — wrapping
/// them anyway is unverified and not worth risking on a field this one
/// isn't hash-gated, so the constraint here stays only "correct", not
/// "byte-stable".
fn mcp_server_command_override(exe: &str) -> String {
    format!("mcp_servers.banto.command=\"{}\"", toml_basic_string(exe))
}

/// `-c mcp_servers.banto.args=...`: the `_mcp` argv this member's server
/// runs with — the brigade-identifying half. Unlike the two overrides
/// above, this one *is* meant to differ by member (`--member`/`--role`) and
/// by launch (`--session`, once discovered) — matches
/// `embedded::emporium::write_mcp_config`'s existing `--role` spelling
/// (`"director"`/`"worker"`) so a Codex member's identity resolves through
/// [`crate::mcp`] the same way a Claude one's does.
pub(crate) fn mcp_server_args_override(brigade: &CodexBrigade) -> String {
    let mut args = vec!["_mcp".to_string()];
    args.extend(brigade.child_args.iter().cloned());
    args.extend([
        "--brigade".to_string(),
        brigade.brigade_id.to_string(),
        "--member".to_string(),
        brigade.token.clone(),
        "--role".to_string(),
        brigade.role.as_token().to_string(),
    ]);
    if let Some(session) = &brigade.session {
        args.push("--session".to_string());
        args.push(session.clone());
    }
    let rendered = args
        .iter()
        .map(|arg| format!("\"{}\"", toml_basic_string(arg)))
        .collect::<Vec<_>>()
        .join(",");
    format!("mcp_servers.banto.args=[{rendered}]")
}

/// `-c mcp_servers.banto.default_tools_approval_mode="approve"`: bypasses
/// the tool-approval prompt for every tool this server offers. Repeated on
/// every launch, deliberately, never written to `~/.codex/config.toml`: a
/// `-c`-registered MCP server can't persist trust at all, and (measured
/// 2026-07-28) the bypass itself degrades silently — no error, no
/// warning — the moment a launch leaves it off.
const MCP_SERVER_APPROVAL_OVERRIDE: &str =
    "mcp_servers.banto.default_tools_approval_mode=\"approve\"";

impl CodexBrigade {
    /// The `-c` overrides this member's launch adds, in the order
    /// [`AgentLaunch::argv`] must emit them — see each override function's
    /// own doc for why this specific one and not a different shape. Normally
    /// four; three when [`hook_command_is_launchable`] rejects `exe` — the
    /// hook override is left out entirely rather than emitted broken (see
    /// that function's own doc). The MCP overrides are unaffected either
    /// way: Codex hands `mcp_servers.banto.*` to its own process spawner
    /// directly rather than splitting them on whitespace itself, so a space
    /// in `exe` is no obstacle for those.
    fn overrides(&self) -> Vec<String> {
        let exe = forward_slash_path(&self.exe);
        let mut overrides = Vec::with_capacity(4);
        if hook_command_is_launchable(&exe) {
            overrides.push(session_start_hook_override(&exe));
        }
        overrides.push(mcp_server_command_override(&exe));
        overrides.push(mcp_server_args_override(self));
        overrides.push(MCP_SERVER_APPROVAL_OVERRIDE.to_string());
        overrides
    }
}

/// `-c tui.notifications=[...]`: the Codex notification events a launch opts
/// into, three of the four its TUI knows. Per Codex's source at tag
/// `rust-v0.168.0` (read, not measured here):
///
/// - `approval-requested`: a tool call waiting on the operator — exec
///   approvals, edit approvals and MCP elicitations.
/// - `async-question`: a question the model asks the user (`request_user_input`).
/// - `plan-mode-prompt`: the plan-implementation prompt.
///
/// All three are moments the session is waiting for a person, which is what a
/// bell here means (the engine turns a BEL from a pane that is not focused into
/// an attention mark). `agent-turn-complete`, the turn-end event, stays out:
/// measured on 0.146.1 (see [`NOTIFICATION_OVERRIDES`]), a plain turn rang
/// nothing with this list as it was, and the bell is meant to be rarer than every
/// turn.
///
/// This is the most fragile of the three overrides below, and the one that
/// breaks with no local edit whatsoever: `--strict-config` (Codex's own
/// config-validation flag) rejects an unknown config *key*, but does not
/// validate the strings inside a list *value* — a typo here, or an upstream
/// rename of an event, passes validation silently. The launch still succeeds,
/// nothing warns, and that prompt simply stops ringing. See
/// [`NOTIFICATION_OVERRIDES`] for the build/date this was last checked
/// against, and why nothing in this repository can catch it going stale; the two
/// names added later were not part of that measurement.
const NOTIFICATION_EVENT_OVERRIDE: &str =
    "tui.notifications=[\"approval-requested\",\"async-question\",\"plan-mode-prompt\"]";

/// `-c tui.notification_method="bel"`: the transport for the event above.
/// Breaks in two different shapes, neither of them a single obvious tidy:
///
/// - Switching to Codex's `auto` looks safe today — `auto` happens to fall
///   back to BEL in this environment — but that is one upstream
///   fallback-logic change away from silently going quiet; nothing local
///   would need to change again for it to break.
/// - Switching to `osc9` looks like the more "correct" choice — Codex's
///   approval notification really is a plain OSC 9 — but it broke
///   immediately, with no upstream change needed at all, when panes were
///   still opened through psmux: psmux models OSC 9;4 (the progress-bar
///   form) but drops any other OSC 9 payload as unmodelled, so a plain-text
///   notification died inside the multiplexer before it ever reached the
///   real terminal a human was looking at. That parser is psmux's own, not
///   part of this repository, so no path here would stay accurate — this is
///   the mechanism, not a citation. Not re-measured against banto's own pane
///   parser.
///
/// See [`NOTIFICATION_OVERRIDES`] for the build/date this was checked
/// against.
const NOTIFICATION_METHOD_OVERRIDE: &str = "tui.notification_method=\"bel\"";

/// `-c tui.notification_condition="always"`: without it, Codex's own
/// documented default (`unfocused`) produces zero bells in any pane banto
/// opens, because Codex considers a banto-embedded pane focused regardless
/// of whether the operator is actually looking at it. Breaks from a single
/// local tidy back to that default — the most fragile-looking failure of
/// the three, even though it needs the most deliberate edit to trigger. See
/// [`NOTIFICATION_OVERRIDES`] for the build/date this was checked against.
const NOTIFICATION_CONDITION_OVERRIDE: &str = "tui.notification_condition=\"always\"";

/// The three `-c` overrides above, injected on every Codex launch
/// unconditionally by [`AgentLaunch::argv`]'s Codex arm — unlike
/// [`CodexBrigade::overrides`], these do not depend on brigade context at
/// all, and land on the ordinary resume/new-session routes too.
///
/// What these overrides make Codex emit is a bare BEL. banto's own vt100
/// parser (`banto_core::screen::Screen`) takes it off the pane's output
/// (`take_audible_bell`), and for a pane that is not focused the engine marks
/// it as needing attention and rings the host terminal (`Cmd::RingHostBell`).
///
/// None of the three values above is verified by anything inside this
/// repository: every external process banto spawns is mocked in tests by
/// design (`docs/DISCIPLINE.md`), so nothing here can catch a value that
/// has quietly stopped doing what its own doc comment says. Re-running
/// against a real `codex` build is the only gate.
///
/// Measured against codex-cli 0.146.1, 2026-08-06, via a PTY harness driving
/// a real Codex turn end to end (not part of this repository — external
/// processes are mocked here by design, see above): a plain turn producing
/// zero bells, then a shell-command turn forced to request approval
/// (`-c approval_policy="untrusted"`) producing exactly one bare `0x07`
/// with `notification_condition="always"` and zero with the condition left
/// at its default — confirming both the suppression `["approval-requested"]`
/// gives over ordinary turn completion and the `unfocused` default's zero
/// bells in a ConPTY-hosted pane. The same shape was first observed on
/// codex-cli 0.146.0 the same day, before the installed build moved out
/// from under this measurement mid-session — two builds agreeing here is
/// why the wording above says "still" and not just "was".
///
/// One thing this run also surfaced, worth knowing rather than assuming:
/// neither of Codex's own launch routes banto uses sets `approval_policy`,
/// and this account's default (with `guardian_approval` stable/on) silently
/// auto-approved a plain workspace-write shell command with no prompt and
/// no notification at all — the override chain here is correct, but how
/// often it actually fires in real use depends on Codex's own approval
/// heuristics, not on anything in this repository.
const NOTIFICATION_OVERRIDES: [&str; 3] = [
    NOTIFICATION_EVENT_OVERRIDE,
    NOTIFICATION_METHOD_OVERRIDE,
    NOTIFICATION_CONDITION_OVERRIDE,
];

impl AgentLaunch {
    /// Render as `binary`'s own argv (see [`agent_binary`] for resolving
    /// it). Claude's fixed order is the one every existing call site already
    /// agreed on before this type existed, `effort` slotted in right after
    /// `--model` since `claude --help` groups the two: `--resume`, then
    /// `--model`, then `--effort`, then `--append-system-prompt`, then
    /// `--mcp-config`. Codex's: the `resume <uuid>` subcommand (or nothing,
    /// for a fresh launch), then `-m`, then the notification `-c` overrides
    /// (always present — see [`NOTIFICATION_OVERRIDES`]), then brigade `-c`
    /// overrides (if any — see [`CodexBrigade::overrides`]), then `-C <cwd>`
    /// last, always present.
    pub(crate) fn argv(&self, binary: &str) -> Vec<String> {
        let mut argv = vec![binary.to_string()];
        match self {
            AgentLaunch::Claude {
                resume,
                model,
                effort,
                permission_mode,
                disallowed_tools,
                append_system_prompt,
                mcp_config,
            } => {
                if let Some(id) = resume {
                    argv.push("--resume".to_string());
                    argv.push(id.clone());
                }
                if let Some(model) = model {
                    argv.push("--model".to_string());
                    argv.push(model.clone());
                }
                if let Some(effort) = effort {
                    argv.push("--effort".to_string());
                    argv.push(effort.clone());
                }
                if let Some(mode) = permission_mode {
                    argv.push("--permission-mode".to_string());
                    argv.push(mode.clone());
                }
                if let Some(tools) = disallowed_tools {
                    argv.push("--disallowedTools".to_string());
                    argv.push(tools.clone());
                }
                if let Some(prompt) = append_system_prompt {
                    argv.push("--append-system-prompt".to_string());
                    argv.push(prompt.clone());
                }
                if let Some(path) = mcp_config {
                    argv.push("--mcp-config".to_string());
                    argv.push(path.to_string_lossy().into_owned());
                }
            }
            AgentLaunch::Codex {
                resume,
                model,
                cwd,
                brigade,
            } => {
                if let Some(id) = resume {
                    argv.push("resume".to_string());
                    argv.push(id.clone());
                }
                if let Some(model) = model {
                    argv.push("-m".to_string());
                    argv.push(model.clone());
                }
                for value in NOTIFICATION_OVERRIDES {
                    argv.push("-c".to_string());
                    argv.push(value.to_string());
                }
                if let Some(brigade) = brigade {
                    for value in brigade.overrides() {
                        argv.push("-c".to_string());
                        argv.push(value);
                    }
                }
                argv.push("-C".to_string());
                argv.push(cwd.to_string_lossy().into_owned());
            }
        }
        argv
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    struct MockProbe {
        alive: HashSet<u32>,
        /// Pids that are alive (per `is_alive`) but whose `is_alive_matching`
        /// must still report `false` regardless of the given `proc_start` —
        /// simulates a pid recycled by an unrelated process since a
        /// live-state file's `proc_start` was recorded.
        identity_mismatch: HashSet<u32>,
    }

    impl MockProbe {
        fn with_alive(pids: &[u32]) -> Self {
            Self {
                alive: pids.iter().copied().collect(),
                identity_mismatch: HashSet::new(),
            }
        }

        /// A probe whose given pids are alive but never match on identity —
        /// see [`Self::identity_mismatch`].
        fn with_alive_but_identity_mismatch(pids: &[u32]) -> Self {
            Self {
                alive: pids.iter().copied().collect(),
                identity_mismatch: pids.iter().copied().collect(),
            }
        }
    }

    impl ProcessProbe for MockProbe {
        fn is_alive(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }

        fn parent_pid(&self, _pid: u32) -> Option<u32> {
            None
        }

        fn is_alive_matching(&self, pid: u32, _proc_start: &str) -> bool {
            self.is_alive(pid) && !self.identity_mismatch.contains(&pid)
        }
    }

    fn session() -> SessionToOpen {
        SessionToOpen {
            id: "sess-1".to_string(),
            agent: AgentKind::ClaudeCode,
            title: "Fix login".to_string(),
            cwd: PathBuf::from("/work/alpha"),
        }
    }

    /// An [`OpenContext`] for tests that don't care about Codex liveness
    /// (`codex_home: None` degrades every Codex check to "not live").
    fn test_ctx<'a>(probe: &'a dyn ProcessProbe, live: &'a [LiveSession]) -> OpenContext<'a> {
        OpenContext {
            probe,
            live,
            codex_home: None,
            start_time: &codex_liveness::SysinfoStartTime,
        }
    }

    /// A synthetic live-state entry, e.g. a session started by hand that's
    /// still running.
    fn live_entry(session_id: &str, pid: u32) -> LiveSession {
        LiveSession {
            pid,
            session_id: Some(session_id.to_string()),
            cwd: None,
            status: None,
            kind: None,
            name: None,
            proc_start: None,
            version: None,
        }
    }

    /// Same as [`live_entry`], but carrying a `proc_start` — exercises
    /// `is_live`'s [`ProcessProbe::is_alive_matching`] path instead of the
    /// bare [`ProcessProbe::is_alive`] one.
    fn live_entry_with_proc_start(session_id: &str, pid: u32, proc_start: &str) -> LiveSession {
        LiveSession {
            proc_start: Some(proc_start.to_string()),
            ..live_entry(session_id, pid)
        }
    }

    #[test]
    fn is_live_matches_only_a_live_entry_with_the_right_session_id_and_a_live_pid() {
        let probe = MockProbe::with_alive(&[100]);

        assert!(is_live(
            &session(),
            &test_ctx(&probe, &[live_entry("sess-1", 100)])
        ));
        // Wrong session id.
        assert!(!is_live(
            &session(),
            &test_ctx(&probe, &[live_entry("sess-2", 100)])
        ));
        // Right session id, dead pid.
        assert!(!is_live(
            &session(),
            &test_ctx(&probe, &[live_entry("sess-1", 999)])
        ));
        // No live entries at all.
        assert!(!is_live(&session(), &test_ctx(&probe, &[])));
    }

    #[test]
    fn is_live_confirms_a_proc_start_carrying_entry_via_is_alive_matching() {
        let probe = MockProbe::with_alive(&[100]);
        let entry = live_entry_with_proc_start("sess-1", 100, "1105463");

        assert!(is_live(&session(), &test_ctx(&probe, &[entry])));
    }

    #[test]
    fn is_live_rejects_a_recycled_pid_even_though_it_is_alive() {
        // The pid is alive per a bare liveness check, but the probe reports
        // it belongs to a different process than the one that recorded
        // `proc_start` (e.g. the original session crashed and the pid was
        // reused) — is_live must not treat this as still running.
        let probe = MockProbe::with_alive_but_identity_mismatch(&[100]);
        let entry = live_entry_with_proc_start("sess-1", 100, "1105463");

        assert!(!is_live(&session(), &test_ctx(&probe, &[entry])));
    }

    #[test]
    fn is_live_codex_session_with_no_codex_home_is_not_live() {
        // codex_home: None degrades every Codex liveness check to "not
        // live" — see `codex_liveness::is_thread_alive`'s doc for why that
        // is the safe direction to fail in.
        let probe = MockProbe::with_alive(&[]);
        let mut codex_session = session();
        codex_session.agent = AgentKind::Codex;

        assert!(!is_live(&codex_session, &test_ctx(&probe, &[])));
    }

    fn blank_claude() -> AgentLaunch {
        AgentLaunch::Claude {
            resume: None,
            model: None,
            effort: None,
            permission_mode: None,
            disallowed_tools: None,
            append_system_prompt: None,
            mcp_config: None,
        }
    }

    #[test]
    fn agent_launch_claude_with_no_fields_is_bare_claude() {
        assert_eq!(
            blank_claude().argv("claude"),
            ["claude"].map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_appends_mcp_config_last_on_its_own() {
        let launch = AgentLaunch::Claude {
            resume: None,
            model: None,
            effort: None,
            permission_mode: None,
            disallowed_tools: None,
            append_system_prompt: None,
            mcp_config: Some(PathBuf::from("C:/data/banto/mcp/1-worker-1.json")),
        };
        assert_eq!(
            launch.argv("claude"),
            [
                "claude",
                "--mcp-config",
                "C:/data/banto/mcp/1-worker-1.json"
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_effort_renders_right_after_model_and_is_absent_when_none() {
        let launch = AgentLaunch::Claude {
            resume: None,
            model: Some("fable".to_string()),
            effort: Some("max".to_string()),
            permission_mode: None,
            disallowed_tools: None,
            append_system_prompt: None,
            mcp_config: None,
        };
        assert_eq!(
            launch.argv("claude"),
            ["claude", "--model", "fable", "--effort", "max"].map(str::to_string)
        );

        let no_effort = AgentLaunch::Claude {
            resume: None,
            model: Some("fable".to_string()),
            effort: None,
            permission_mode: None,
            disallowed_tools: None,
            append_system_prompt: None,
            mcp_config: None,
        };
        assert_eq!(
            no_effort.argv("claude"),
            ["claude", "--model", "fable"].map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_permission_mode_renders_right_after_effort_and_is_absent_when_none() {
        let launch = AgentLaunch::Claude {
            resume: None,
            model: Some("fable".to_string()),
            effort: Some("max".to_string()),
            permission_mode: Some("auto".to_string()),
            disallowed_tools: None,
            append_system_prompt: None,
            mcp_config: None,
        };
        assert_eq!(
            launch.argv("claude"),
            [
                "claude",
                "--model",
                "fable",
                "--effort",
                "max",
                "--permission-mode",
                "auto"
            ]
            .map(str::to_string)
        );

        let no_permission_mode = AgentLaunch::Claude {
            resume: None,
            model: Some("fable".to_string()),
            effort: Some("max".to_string()),
            permission_mode: None,
            disallowed_tools: None,
            append_system_prompt: None,
            mcp_config: None,
        };
        assert_eq!(
            no_permission_mode.argv("claude"),
            ["claude", "--model", "fable", "--effort", "max"].map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_disallowed_tools_renders_right_after_permission_mode_and_is_absent_when_none() {
        let launch = AgentLaunch::Claude {
            resume: None,
            model: Some("fable".to_string()),
            effort: Some("max".to_string()),
            permission_mode: Some("auto".to_string()),
            disallowed_tools: Some("Edit,Write,NotebookEdit".to_string()),
            append_system_prompt: None,
            mcp_config: None,
        };
        assert_eq!(
            launch.argv("claude"),
            [
                "claude",
                "--model",
                "fable",
                "--effort",
                "max",
                "--permission-mode",
                "auto",
                "--disallowedTools",
                "Edit,Write,NotebookEdit"
            ]
            .map(str::to_string)
        );

        let no_disallowed_tools = AgentLaunch::Claude {
            resume: None,
            model: Some("fable".to_string()),
            effort: Some("max".to_string()),
            permission_mode: Some("auto".to_string()),
            disallowed_tools: None,
            append_system_prompt: None,
            mcp_config: None,
        };
        assert_eq!(
            no_disallowed_tools.argv("claude"),
            [
                "claude",
                "--model",
                "fable",
                "--effort",
                "max",
                "--permission-mode",
                "auto"
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_combines_every_flag_in_the_fixed_order() {
        let launch = AgentLaunch::Claude {
            resume: Some("sess-1".to_string()),
            model: Some("opus".to_string()),
            effort: Some("high".to_string()),
            permission_mode: Some("auto".to_string()),
            disallowed_tools: Some("Edit,Write,NotebookEdit".to_string()),
            append_system_prompt: Some("you are the Director".to_string()),
            mcp_config: Some(PathBuf::from("C:/data/banto/mcp/1-director.json")),
        };
        assert_eq!(
            launch.argv("claude"),
            [
                "claude",
                "--resume",
                "sess-1",
                "--model",
                "opus",
                "--effort",
                "high",
                "--permission-mode",
                "auto",
                "--disallowedTools",
                "Edit,Write,NotebookEdit",
                "--append-system-prompt",
                "you are the Director",
                "--mcp-config",
                "C:/data/banto/mcp/1-director.json",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_codex_fresh_launch_carries_the_notification_overrides_plus_cwd() {
        let launch = AgentLaunch::Codex {
            resume: None,
            model: None,
            cwd: PathBuf::from("/work/alpha"),
            brigade: None,
        };
        assert_eq!(
            launch.argv("codex"),
            [
                "codex",
                "-c",
                "tui.notifications=[\"approval-requested\",\"async-question\",\"plan-mode-prompt\"]",
                "-c",
                "tui.notification_method=\"bel\"",
                "-c",
                "tui.notification_condition=\"always\"",
                "-C",
                "/work/alpha",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_codex_resume_renders_the_subcommand_then_model_then_notifications_then_cwd() {
        let launch = AgentLaunch::Codex {
            resume: Some("codex-uuid-1".to_string()),
            model: Some("o3".to_string()),
            cwd: PathBuf::from("/work/alpha"),
            brigade: None,
        };
        assert_eq!(
            launch.argv("codex"),
            [
                "codex",
                "resume",
                "codex-uuid-1",
                "-m",
                "o3",
                "-c",
                "tui.notifications=[\"approval-requested\",\"async-question\",\"plan-mode-prompt\"]",
                "-c",
                "tui.notification_method=\"bel\"",
                "-c",
                "tui.notification_condition=\"always\"",
                "-C",
                "/work/alpha",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_codex_brigade_inserts_notification_then_brigade_overrides_between_model_and_cwd()
     {
        let launch = AgentLaunch::Codex {
            resume: None,
            model: None,
            cwd: PathBuf::from("/work/alpha"),
            brigade: Some(CodexBrigade {
                exe: PathBuf::from("/opt/banto/banto"),
                brigade_id: 1,
                token: "worker-1".to_string(),
                role: BrigadeRole::Worker,
                session: None,
                child_args: Vec::new(),
            }),
        };
        assert_eq!(
            launch.argv("codex"),
            [
                "codex",
                "-c",
                "tui.notifications=[\"approval-requested\",\"async-question\",\"plan-mode-prompt\"]",
                "-c",
                "tui.notification_method=\"bel\"",
                "-c",
                "tui.notification_condition=\"always\"",
                "-c",
                "hooks.SessionStart=[{matcher=\"\",hooks=[{type=\"command\",timeout_sec=600,command=\"/opt/banto/banto _hook\"}]}]",
                "-c",
                "mcp_servers.banto.command=\"/opt/banto/banto\"",
                "-c",
                "mcp_servers.banto.args=[\"_mcp\",\"--brigade\",\"1\",\"--member\",\"worker-1\",\"--role\",\"worker\"]",
                "-c",
                "mcp_servers.banto.default_tools_approval_mode=\"approve\"",
                "-C",
                "/work/alpha",
            ]
            .map(str::to_string)
        );
    }

    #[test]
    fn agent_launch_codex_without_a_brigade_still_carries_the_three_notification_overrides() {
        // The case that does not exist today: an ordinary (non-brigade)
        // Codex launch used to emit no `-c` at all. It must now carry
        // exactly the three notification overrides, and nothing else that
        // brigade context would have added (no hook, no MCP server).
        let launch = AgentLaunch::Codex {
            resume: Some("codex-uuid-1".to_string()),
            model: Some("o3".to_string()),
            cwd: PathBuf::from("/work/alpha"),
            brigade: None,
        };
        let argv = launch.argv("codex");
        assert_eq!(
            argv.iter().filter(|arg| arg.as_str() == "-c").count(),
            3,
            "expected exactly the three notification overrides: {argv:?}"
        );
        for value in NOTIFICATION_OVERRIDES {
            assert!(
                argv.contains(&value.to_string()),
                "missing {value:?} in {argv:?}"
            );
        }
        assert!(!argv.iter().any(|arg| arg.contains("mcp_servers")));
        assert!(!argv.iter().any(|arg| arg.contains("hooks.SessionStart")));
    }

    #[test]
    fn codex_brigade_overrides_normalize_a_windows_exe_path_to_forward_slashes() {
        // Pins the exact `-c` text, not just "it round-trips": hook trust
        // is a hash of overrides()[0], so this string's shape must not
        // drift by even a byte once it lands (see
        // `session_start_hook_override`'s doc).
        let brigade = CodexBrigade {
            exe: PathBuf::from(r#"C:\Users\kt81\banto-dogfood\banto.exe"#),
            brigade_id: 1,
            token: "director".to_string(),
            role: BrigadeRole::Director,
            session: None,
            child_args: Vec::new(),
        };
        let overrides = brigade.overrides();
        assert_eq!(
            overrides[0],
            r#"hooks.SessionStart=[{matcher="",hooks=[{type="command",timeout_sec=600,command="C:/Users/kt81/banto-dogfood/banto.exe _hook"}]}]"#
        );
        assert_eq!(
            overrides[1],
            r#"mcp_servers.banto.command="C:/Users/kt81/banto-dogfood/banto.exe""#
        );
    }

    #[test]
    fn codex_brigade_omits_the_hook_override_when_the_exe_path_contains_a_space() {
        // The measured failure mode this guards: quoting doesn't save a
        // spaced path either (see `hook_command_is_launchable`'s doc) — the
        // only fix left is to never emit a hook `-c` that can never fire.
        // The MCP overrides still go out: those aren't split on whitespace
        // by Codex, so the space is no obstacle for them.
        let brigade = CodexBrigade {
            exe: PathBuf::from(r#"C:\Program Files\banto\banto.exe"#),
            brigade_id: 1,
            token: "director".to_string(),
            role: BrigadeRole::Director,
            session: None,
            child_args: Vec::new(),
        };
        let overrides = brigade.overrides();
        assert_eq!(overrides.len(), 3);
        assert!(!overrides.iter().any(|o| o.contains("SessionStart")));
        assert_eq!(
            overrides[0],
            r#"mcp_servers.banto.command="C:/Program Files/banto/banto.exe""#
        );
    }

    #[test]
    fn toml_basic_string_escapes_backslash_before_quote_not_after() {
        // A literal `"` must become `\"`, not `\\"` — proves the two
        // replace passes run in the order the doc promises.
        assert_eq!(toml_basic_string(r#"a"b"#), r#"a\"b"#);
        assert_eq!(toml_basic_string(r"a\b"), r"a\\b");
    }

    #[test]
    fn codex_brigade_hook_and_mcp_command_overrides_are_identical_across_members() {
        // Hook trust is a hash of overrides()[0]'s exact text (see its own
        // doc) — it must not vary with brigade id, token, role, or session,
        // or every member drops back to untrusted with no warning at all.
        let director = CodexBrigade {
            exe: PathBuf::from("/opt/banto/banto"),
            brigade_id: 1,
            token: "director".to_string(),
            role: BrigadeRole::Director,
            session: Some("dir-session".to_string()),
            child_args: Vec::new(),
        };
        let worker = CodexBrigade {
            exe: PathBuf::from("/opt/banto/banto"),
            brigade_id: 7,
            token: "worker-1".to_string(),
            role: BrigadeRole::Worker,
            session: None,
            child_args: Vec::new(),
        };
        assert_eq!(director.overrides()[0], worker.overrides()[0]);
        // The MCP `command` override shares the same reasoning, even though
        // it isn't hash-gated.
        assert_eq!(director.overrides()[1], worker.overrides()[1]);
        // The identity-carrying `args` override, and only that one, differs.
        assert_ne!(director.overrides()[2], worker.overrides()[2]);
        assert_eq!(director.overrides()[3], worker.overrides()[3]);
    }

    #[test]
    fn codex_brigade_mcp_args_include_session_only_when_known() {
        let without_session = CodexBrigade {
            exe: PathBuf::from("/opt/banto/banto"),
            brigade_id: 1,
            token: "worker-1".to_string(),
            role: BrigadeRole::Worker,
            session: None,
            child_args: Vec::new(),
        };
        let with_session = CodexBrigade {
            session: Some("w1-session".to_string()),
            ..without_session.clone()
        };
        assert_eq!(
            without_session.overrides()[2],
            "mcp_servers.banto.args=[\"_mcp\",\"--brigade\",\"1\",\"--member\",\"worker-1\",\"--role\",\"worker\"]"
        );
        assert_eq!(
            with_session.overrides()[2],
            "mcp_servers.banto.args=[\"_mcp\",\"--brigade\",\"1\",\"--member\",\"worker-1\",\"--role\",\"worker\",\"--session\",\"w1-session\"]"
        );
    }

    #[test]
    fn agent_binary_falls_back_to_the_bare_name_when_unset() {
        let binaries = AgentBinaries::default();
        assert_eq!(agent_binary(AgentKind::ClaudeCode, &binaries), "claude");
        assert_eq!(agent_binary(AgentKind::Codex, &binaries), "codex");
    }

    #[test]
    fn agent_binary_uses_the_configured_override_when_set() {
        let binaries = AgentBinaries {
            claude: None,
            codex: Some(PathBuf::from("C:/tools/codex.exe")),
        };
        assert_eq!(agent_binary(AgentKind::ClaudeCode, &binaries), "claude");
        assert_eq!(
            agent_binary(AgentKind::Codex, &binaries),
            "C:/tools/codex.exe"
        );
    }

    #[test]
    fn a_configured_binary_is_the_launchs_argv0() {
        let binaries = AgentBinaries {
            claude: None,
            codex: Some(PathBuf::from("C:/tools/codex.exe")),
        };
        let launch = AgentLaunch::Codex {
            resume: Some("codex-uuid-1".to_string()),
            model: None,
            cwd: PathBuf::from("/work/alpha"),
            brigade: None,
        };
        let argv = launch.argv(&agent_binary(AgentKind::Codex, &binaries));
        // The configured Codex binary, not the bare "codex" default.
        assert_eq!(argv[0], "C:/tools/codex.exe");
        assert_eq!(argv[1], "resume");
    }

    fn parent_args() -> Vec<String> {
        vec![
            "--parent-config".to_string(),
            "/my config/banto.toml".to_string(),
            "--db-path".to_string(),
            r"C:\Users\me\My Data\banto.db".to_string(),
        ]
    }

    #[test]
    fn a_codex_members_mcp_server_gets_the_parents_resolution_and_its_hook_is_unchanged() {
        let plain = CodexBrigade {
            exe: PathBuf::from("/opt/banto/banto"),
            brigade_id: 1,
            token: "worker-1".to_string(),
            role: BrigadeRole::Worker,
            session: None,
            child_args: Vec::new(),
        };
        let handed = CodexBrigade {
            child_args: parent_args(),
            ..plain.clone()
        };
        // Codex trusts the hook by a hash of this exact string: it must not depend
        // on anything the parent hands over.
        assert_eq!(handed.overrides()[0], plain.overrides()[0]);
        assert_eq!(handed.overrides()[1], plain.overrides()[1]);
        let args = handed.overrides()[2].clone();
        let rhs = args.strip_prefix("mcp_servers.banto.args=").unwrap();
        let parsed: toml::Table = toml::from_str(&format!("args = {rhs}")).unwrap();
        let args: Vec<&str> = parsed["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        // Every value comes back whole and byte for byte, backslashes and spaces
        // included, and the server's own flags follow.
        assert_eq!(
            args[..5],
            [
                "_mcp",
                "--parent-config",
                "/my config/banto.toml",
                "--db-path",
                r"C:\Users\me\My Data\banto.db"
            ]
        );
        assert_eq!(
            args[5..],
            ["--brigade", "1", "--member", "worker-1", "--role", "worker"]
        );
    }
}
