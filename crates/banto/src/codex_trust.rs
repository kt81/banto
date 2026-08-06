//! `trust_argv`: the argv a solo pane runs to let the operator review and
//! approve banto's own `SessionStart` hook, before any brigade launch needs
//! it trusted.
//!
//! Codex reviews hook trust once, at its own startup, from
//! `config.toml`/`-c` — not per turn. So the first time a brigade with a
//! Codex member forms, every one of its panes launches Codex in the same
//! instant, and every one shows its own trust-review dialog at once:
//! approving it in the Director's pane never reaches a Worker that started
//! alongside it, and choosing "Continue without trusting" to get past a
//! pane quickly leaves that member silently briefing-less — worse, that
//! choice doesn't persist, so the same pane asks again next launch too.
//! `Modal::ConfirmCodexTrust` (opened by `banto_core::engine` the moment a
//! brigade formation resolves to a Codex Worker and the hook doesn't look
//! trusted — see `update_codex_trust_checked`'s doc) moves the one approval
//! this needs to a deliberate moment before formation, so every later
//! brigade launch finds it already trusted. This used to be a standalone
//! `banto codex-trust` CLI subcommand; it was removed once the operator
//! could be asked the same question from inside the TUI, since a CLI
//! command depended on the operator remembering to run it from the same
//! executable copy banto itself launches brigades from — `std::env::
//! current_exe()` inside a running banto can't be pointed at the wrong copy
//! the way a separately-typed command line could be.

use std::path::Path;

use banto_core::config::AgentBinaries;
use banto_core::model::AgentKind;

use crate::opener::{agent_binary, forward_slash_path, session_start_hook_override};

/// The argv the trust-review pane runs: `codex -c <hook override>`, nothing
/// else. No `mcp_servers.banto.*` overrides — trust is only ever asked for
/// the hook; MCP tool approval is solved a different way entirely, per
/// launch, via `default_tools_approval_mode` (see
/// `opener::mcp_server_command_override`'s doc) — and no `-C`/`resume`:
/// this isn't opening any particular session, just Codex's own startup
/// trust prompt. Reuses [`session_start_hook_override`] rather than
/// building its own copy of the string — see that function's own doc for
/// why a second implementation here would be worse than none at all.
///
/// Also no `opener::NOTIFICATION_OVERRIDES`, for the same reason as the MCP
/// overrides: this pane exists only to show Codex's own startup trust
/// prompt. There is no turn to complete and no approval to request here, so
/// a notification has no addressee.
///
/// `pub(crate)`: `crate::embedded::emporium::execute_open_codex_trust_pane`
/// is the only caller now that this isn't a CLI subcommand anymore, but it
/// lives in a different module.
pub(crate) fn trust_argv(exe: &Path, binaries: &AgentBinaries) -> Vec<String> {
    vec![
        agent_binary(AgentKind::Codex, binaries),
        "-c".to_string(),
        session_start_hook_override(&forward_slash_path(exe)),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use banto_core::model::BrigadeRole;

    use super::*;
    use crate::opener::{AgentLaunch, CodexBrigade};

    #[test]
    fn trust_argv_hook_override_matches_a_real_brigade_launch_byte_for_byte() {
        // The whole point of this pane: what the operator trusts here must
        // be exactly what a real brigade launch sends, or the approval they
        // just gave doesn't cover it.
        let exe = PathBuf::from(r"C:\Users\kt81\banto-dogfood\banto.exe");
        let binaries = AgentBinaries::default();

        let trust_hook = trust_argv(&exe, &binaries)[2].clone();

        let launch = AgentLaunch::Codex {
            resume: None,
            model: None,
            cwd: PathBuf::from("/work/anywhere"),
            brigade: Some(CodexBrigade {
                exe: exe.clone(),
                brigade_id: 1,
                token: "worker-1".to_string(),
                role: BrigadeRole::Worker,
                session: None,
            }),
        };

        // Found by content, not position: the notification overrides now
        // sit ahead of the brigade overrides in a real launch's argv, so
        // the hook string no longer lands at a fixed index.
        assert!(launch.argv("codex").contains(&trust_hook));
    }

    #[test]
    fn trust_argv_never_includes_an_mcp_override() {
        let argv = trust_argv(
            &PathBuf::from("/opt/banto/banto"),
            &AgentBinaries::default(),
        );
        assert!(!argv.iter().any(|arg| arg.contains("mcp_servers")));
    }

    #[test]
    fn trust_argv_is_exactly_codex_dash_c_hook_nothing_else() {
        let argv = trust_argv(
            &PathBuf::from("/opt/banto/banto"),
            &AgentBinaries::default(),
        );
        assert_eq!(
            argv,
            vec![
                "codex".to_string(),
                "-c".to_string(),
                session_start_hook_override("/opt/banto/banto"),
            ]
        );
    }

    #[test]
    fn trust_argv_uses_the_configured_codex_binary_override() {
        let binaries = AgentBinaries {
            claude: None,
            codex: Some(PathBuf::from("C:/tools/codex.exe")),
        };
        let argv = trust_argv(&PathBuf::from("/opt/banto/banto"), &binaries);
        assert_eq!(argv[0], "C:/tools/codex.exe");
    }
}
