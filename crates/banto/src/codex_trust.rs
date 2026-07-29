//! `banto codex-trust`: a standalone `codex` session for approving the
//! `SessionStart` hook banto uses to brief a Codex brigade member, before
//! any real brigade launch needs it.
//!
//! Codex reviews hook trust once, at its own startup, from
//! `config.toml`/`-c` — not per turn. So the first time a brigade with a
//! Codex member forms, every one of its panes launches Codex in the same
//! instant, and every one shows its own trust-review dialog at once:
//! approving it in the Director's pane never reaches a Worker that started
//! alongside it, and choosing "Continue without trusting" to get past a
//! pane quickly leaves that member silently briefing-less — worse, that
//! choice doesn't persist, so the same pane asks again next launch too.
//! `codex-trust` moves the one approval this needs to a deliberate moment
//! before formation, so every later brigade launch finds it already
//! trusted.

use std::io;
use std::path::Path;

use banto_core::config::AgentBinaries;
use banto_core::model::AgentKind;
use banto_io::process::ProcessRunner;

use crate::opener::{agent_binary, forward_slash_path, session_start_hook_override};

/// Run `codex -c <hook override>` interactively (inherited stdio, blocking
/// until it exits — see `runner`'s own trait doc), after a short
/// explanation of what's about to happen. `exe` is banto's own executable
/// path (production callers pass `std::env::current_exe()`, injected here
/// so this stays deterministic in tests — the same convention
/// `opener::wrap_argv`'s own `exe` parameter already uses).
pub(crate) fn run(
    exe: &Path,
    binaries: &AgentBinaries,
    runner: &dyn ProcessRunner,
) -> io::Result<Option<i32>> {
    println!(
        "banto: starting Codex so you can review and trust its SessionStart hook.\n\
         Choose \"Trust all and continue\", then run /quit once Codex is ready \
         — nothing else to do here."
    );
    let code = runner.run(&trust_argv(exe, binaries))?;
    println!("banto: done — Codex brigade members can be started normally now.");
    Ok(code)
}

/// The argv this subcommand runs: `codex -c <hook override>`, nothing
/// else. No `mcp_servers.banto.*` overrides — trust is only ever asked for
/// the hook; MCP tool approval is solved a different way entirely, per
/// launch, via `default_tools_approval_mode` (see
/// `opener::mcp_server_command_override`'s doc) — and no `-C`/`resume`:
/// this isn't opening any particular session, just Codex's own startup
/// trust prompt. Reuses [`session_start_hook_override`] rather than
/// building its own copy of the string — see that function's own doc for
/// why a second implementation here would be worse than none at all.
fn trust_argv(exe: &Path, binaries: &AgentBinaries) -> Vec<String> {
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
    use banto_io::process::mock::MockProcessRunner;

    use super::*;
    use crate::opener::{AgentLaunch, CodexBrigade};

    #[test]
    fn trust_argv_hook_override_matches_a_real_brigade_launch_byte_for_byte() {
        // The whole point of this subcommand: what the operator trusts
        // here must be exactly what a real brigade launch sends, or the
        // approval they just gave doesn't cover it.
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
        let brigade_hook = launch.argv("codex")[2].clone();

        assert_eq!(trust_hook, brigade_hook);
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

    #[test]
    fn run_invokes_the_process_runner_with_the_trust_argv_and_returns_its_exit_code() {
        let exe = PathBuf::from("/opt/banto/banto");
        let binaries = AgentBinaries::default();
        let runner = MockProcessRunner::new(Some(0));

        let code = run(&exe, &binaries, &runner).unwrap();

        assert_eq!(code, Some(0));
        assert_eq!(runner.calls(), vec![trust_argv(&exe, &binaries)]);
    }
}
