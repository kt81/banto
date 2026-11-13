//! A process banto starts must settle on the same config file, database and
//! agent homes as the banto that started it, whatever its own environment and
//! working directory are.
//!
//! Each test names a parent (an explicit `--config`, a `db_path` in it, home
//! overrides — relative ones and ones with a space in them), takes every child
//! invocation exactly as banto builds it, and resolves it the way that child's
//! own code does: parsed by the real command line, in another working directory,
//! with no `BANTO_CONFIG` inherited. Nothing here touches a real directory: the
//! "platform" locations are all inside a temp dir.

use std::path::{Path, PathBuf};

use clap::Parser;

use banto_core::model::BrigadeRole;

use super::*;
use crate::Cli;

/// A throwaway machine: a temp dir holding the parent's and the child's working
/// directories, a fake home, and the platform locations (none of which exists).
struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    parent_cwd: PathBuf,
    child_cwd: PathBuf,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let parent_cwd = root.join("parent cwd");
        let child_cwd = root.join("session dir");
        std::fs::create_dir_all(&parent_cwd).unwrap();
        std::fs::create_dir_all(&child_cwd).unwrap();
        Self {
            _tmp: tmp,
            root,
            parent_cwd,
            child_cwd,
        }
    }

    /// The environment of either process: nothing banto reads is set, and the
    /// platform locations are inside the temp dir and absent.
    fn env(&self) -> Env {
        Env {
            home: Some(self.root.join("fake home")),
            default_config: Some(self.root.join("platform config").join("config.toml")),
            data_dir: Some(self.root.join("platform data")),
            ..Env::default()
        }
    }

    /// The environment of a child: the same machine, except that its own idea of
    /// where the platform's data directory is differs from the parent's (another
    /// `XDG_DATA_HOME` / profile) — which is exactly what the hand-off must
    /// override.
    fn child_env(&self) -> Env {
        Env {
            data_dir: Some(self.root.join("a different data dir")),
            ..self.env()
        }
    }

    /// The parent: `--config "conf dir/banto.toml"` (relative to its working
    /// directory), whose file names a relative `db_path`; `--claude-home` and
    /// `--codex-home` given relative, the Codex one with a space in it.
    fn parent(&self) -> (Config, Resolved) {
        self.parent_in(self.env())
    }

    /// [`Self::parent`] in another environment (one with no data directory, say).
    fn parent_in(&self, env: Env) -> (Config, Resolved) {
        let conf = self.parent_cwd.join("conf dir");
        std::fs::create_dir_all(&conf).unwrap();
        std::fs::write(
            conf.join("banto.toml"),
            "db_path = \"state dir/banto.db\"\n",
        )
        .unwrap();
        let inputs = Inputs {
            cli_config: Some(PathBuf::from("conf dir/banto.toml")),
            cli_claude_home: Some(PathBuf::from("homes/claude")),
            cli_codex_home: Some(PathBuf::from("homes/codex home")),
            env,
            cwd: Some(self.parent_cwd.clone()),
            ..Inputs::default()
        };
        resolve(&inputs, Strictness::Strict).unwrap()
    }

    /// The same parent, but started with a relative `BANTO_CONFIG` in its
    /// environment instead of `--config`.
    fn parent_from_env(&self) -> (Config, Resolved) {
        let conf = self.parent_cwd.join("conf dir");
        std::fs::create_dir_all(&conf).unwrap();
        std::fs::write(
            conf.join("banto.toml"),
            "db_path = \"state dir/banto.db\"\nclaude_home = \"homes/claude\"\n",
        )
        .unwrap();
        let mut env = self.env();
        env.banto_config = Some(PathBuf::from("conf dir/banto.toml"));
        let inputs = Inputs {
            env,
            cwd: Some(self.parent_cwd.clone()),
            ..Inputs::default()
        };
        resolve(&inputs, Strictness::Strict).unwrap()
    }

    /// What a child that is handed `argv` (everything after the executable)
    /// resolves, from its own working directory and the environment of the
    /// world — with no `BANTO_CONFIG` unless the test says it was inherited.
    fn resolve_argv(&self, argv: &[String], inherited_config: Option<&str>) -> Result<Resolved> {
        let mut full = vec!["banto".to_string()];
        full.extend(argv.iter().cloned());
        let cli = Cli::try_parse_from(&full)
            .unwrap_or_else(|err| panic!("the child's argv does not parse: {err}\n{full:?}"));
        let mut env = self.child_env();
        env.banto_config = inherited_config.map(PathBuf::from);
        let inputs = Inputs {
            cli_config: cli.config.clone(),
            cli_claude_home: cli.claude_home.clone(),
            cli_codex_home: cli.codex_home.clone(),
            cli_db_path: cli.db_path.clone(),
            cli_data_dir: cli.data_dir.clone(),
            parent_config: cli.parent_config.clone(),
            parent_no_config: cli.parent_no_config,
            env,
            cwd: Some(self.child_cwd.clone()),
        };
        resolve(&inputs, Strictness::Strict).map(|(_, resolved)| resolved)
    }

    /// What the `_hook` process resolves from the environment banto gave it.
    fn resolve_hook(&self, given: &[(String, String)], inherited_config: Option<&str>) -> Resolved {
        let mut env = self.child_env();
        env.banto_config = inherited_config.map(PathBuf::from);
        for (key, value) in given {
            match key.as_str() {
                "BANTO_CONFIG" => env.banto_config = Some(PathBuf::from(value)),
                "CLAUDE_CONFIG_DIR" => env.claude_config_dir = Some(value.clone()),
                "CODEX_HOME" => env.codex_home = Some(value.clone()),
                _ => {}
            }
        }
        let mut inputs = Inputs {
            env,
            cwd: Some(self.child_cwd.clone()),
            ..Inputs::default()
        };
        // What `_hook` does with the variables it was given.
        inputs.apply_parent_env(|key| {
            given
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        });
        resolve(&inputs, Strictness::Lenient).unwrap().1
    }
}

/// What a parent hands to a child: for now, what banto builds today.
struct Invocations {
    claude_mcp: Vec<String>,
    codex_mcp: Vec<String>,
    wrap_resume: Vec<String>,
    wrap_new_session: Vec<String>,
    hook_env: Vec<(String, String)>,
}

fn invocations(parent: &Resolved) -> Invocations {
    let child_args = parent.child_args();
    let exe = "/opt/banto/banto";
    let brigade = (7, "worker-1".to_string(), BrigadeRole::Worker);

    let json = crate::embedded::mcp_config_json(
        Path::new(exe),
        &child_args,
        7,
        "worker-1",
        BrigadeRole::Worker,
        Some("sess-1"),
    );
    let claude_mcp = json["mcpServers"]["banto"]["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_string())
        .collect();

    let codex = crate::opener::CodexBrigade {
        exe: PathBuf::from(exe),
        brigade_id: 7,
        token: "worker-1".to_string(),
        role: BrigadeRole::Worker,
        session: Some("sess-1".to_string()),
        child_args: child_args.clone(),
    };
    let override_ = crate::opener::mcp_server_args_override(&codex);
    let rhs = override_.strip_prefix("mcp_servers.banto.args=").unwrap();
    let parsed: toml::Table = toml::from_str(&format!("args = {rhs}")).unwrap();
    let codex_mcp = parsed["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_string())
        .collect();

    let binaries = banto_core::config::AgentBinaries::default();
    let wrap_resume = crate::opener::wrap_argv(
        Some(exe),
        &child_args,
        "sess-1",
        banto_core::model::AgentKind::ClaudeCode,
        Path::new("/work"),
        &binaries,
    );
    let wrap_new_session = crate::opener::new_session_wrap_argv(
        Some(exe),
        &child_args,
        Path::new("/work"),
        banto_io::opener::Backend::Psmux,
        None,
        &binaries,
    );
    Invocations {
        claude_mcp,
        codex_mcp,
        wrap_resume: wrap_resume[1..].to_vec(),
        wrap_new_session: wrap_new_session[1..].to_vec(),
        hook_env: crate::embedded::brigade_env(Some(&brigade), Some(parent)),
    }
}

/// Every difference at once, so a failure shows all of what disagrees.
fn assert_agrees(label: &str, parent: &Resolved, child: &Resolved, homes: bool) {
    let mut differences = Vec::new();
    let mut check = |what: &str, parent: String, child: String| {
        if parent != child {
            differences.push(format!(
                "  {what}:
    parent: {parent}
    child:  {child}"
            ));
        }
    };
    check(
        "config file",
        format!("{:?}", parent.config_file),
        format!("{:?}", child.config_file),
    );
    check(
        "database",
        format!("{:?}", parent.db_path),
        format!("{:?}", child.db_path),
    );
    if homes {
        check(
            "data directory",
            format!("{:?}", parent.data_dir),
            format!("{:?}", child.data_dir),
        );
        // Everything banto derives from it: the MCP configs (and where a member's
        // goes), the consultation requests, the message logs.
        let derived = |r: &Resolved| {
            r.data_dir().map(|d| {
                (
                    d.mcp_dir(),
                    d.member_config_path(7, "worker-1"),
                    d.goinkyo_dir(),
                    d.goinkyo_request_path(7),
                    d.logs_dir(),
                )
            })
        };
        check(
            "paths derived from the data directory",
            format!("{:?}", derived(parent)),
            format!("{:?}", derived(child)),
        );
        check(
            "Claude home",
            format!("{:?}", parent.claude_home),
            format!("{:?}", child.claude_home),
        );
        check(
            "Codex home",
            format!("{:?}", parent.codex_home),
            format!("{:?}", child.codex_home),
        );
    }
    assert!(
        differences.is_empty(),
        "{label} resolves differently from its parent:
{}",
        differences.join(
            "
"
        )
    );
}

#[test]
fn the_parent_makes_every_relative_input_absolute_against_its_own_directory() {
    let world = World::new();
    let (_, parent) = world.parent();
    assert_eq!(
        parent.config_file,
        ConfigFile::File(world.parent_cwd.join("conf dir/banto.toml"))
    );
    assert_eq!(
        parent.db_path,
        Some(world.parent_cwd.join("state dir/banto.db"))
    );
    assert_eq!(
        parent.claude_home,
        Some(world.parent_cwd.join("homes/claude"))
    );
    assert_eq!(
        parent.codex_home,
        Some(world.parent_cwd.join("homes/codex home"))
    );
}

#[test]
fn a_claude_mcp_server_resolves_what_its_parent_did() {
    let world = World::new();
    let (_, parent) = world.parent();
    let child = world
        .resolve_argv(&invocations(&parent).claude_mcp, None)
        .unwrap_or_else(|err| panic!("claude _mcp cannot start: {err:#}"));
    assert_agrees("claude _mcp", &parent, &child, true);
}

#[test]
fn a_codex_mcp_server_resolves_what_its_parent_did() {
    let world = World::new();
    let (_, parent) = world.parent();
    let child = world
        .resolve_argv(&invocations(&parent).codex_mcp, None)
        .unwrap_or_else(|err| panic!("codex _mcp cannot start: {err:#}"));
    assert_agrees("codex _mcp", &parent, &child, true);
}

#[test]
fn a_wrap_that_resumes_a_session_resolves_what_its_parent_did() {
    let world = World::new();
    let (_, parent) = world.parent();
    let child = world
        .resolve_argv(&invocations(&parent).wrap_resume, None)
        .unwrap_or_else(|err| panic!("_wrap resume cannot start: {err:#}"));
    assert_agrees("_wrap resume", &parent, &child, true);
}

#[test]
fn a_wrap_that_starts_a_new_session_resolves_what_its_parent_did() {
    let world = World::new();
    let (_, parent) = world.parent();
    let child = world
        .resolve_argv(&invocations(&parent).wrap_new_session, None)
        .unwrap_or_else(|err| panic!("_wrap new-session cannot start: {err:#}"));
    assert_agrees("_wrap new-session", &parent, &child, true);
}

/// A parent with no data directory (the platform has none) passes none, and a
/// child handed anything else must not find one for itself — as with the Codex
/// home, where the parent had none the child has none. The child's own platform
/// does have one here (`child_env`), which is what it must not use.
#[test]
fn children_of_a_parent_with_no_data_directory_have_none_either() {
    let world = World::new();
    let (_, parent) = world.parent_in(Env {
        data_dir: None,
        ..world.env()
    });
    assert_eq!(parent.data_dir, None);
    let invocations = invocations(&parent);
    assert!(
        !invocations.claude_mcp.iter().any(|arg| arg == "--data-dir"),
        "{:?}",
        invocations.claude_mcp
    );
    for (label, argv) in [
        ("claude _mcp", &invocations.claude_mcp),
        ("codex _mcp", &invocations.codex_mcp),
        ("_wrap resume", &invocations.wrap_resume),
        ("_wrap new-session", &invocations.wrap_new_session),
    ] {
        let child = world
            .resolve_argv(argv, None)
            .unwrap_or_else(|err| panic!("{label} cannot start: {err:#}"));
        assert_eq!(child.data_dir, None, "{label}");
        assert_agrees(label, &parent, &child, true);
    }
    // The hook is handed through the environment and reads no data directory:
    // it has none either, and its database is still the parent's.
    let hook = world.resolve_hook(&invocations.hook_env, None);
    assert_eq!(hook.data_dir, None);
    assert_agrees("_hook", &parent, &hook, false);
}

#[test]
fn a_hook_resolves_the_config_and_database_its_parent_did() {
    let world = World::new();
    let (_, parent) = world.parent();
    let child = world.resolve_hook(&invocations(&parent).hook_env, None);
    // The hook reads no agent home.
    assert_agrees("_hook", &parent, &child, false);
}

/// The other way a parent gets its config: a relative `BANTO_CONFIG`. A child
/// inherits that variable verbatim — and a relative path means another file from
/// the session's directory.
#[test]
fn children_of_a_parent_started_from_a_relative_banto_config_resolve_what_it_did() {
    let world = World::new();
    let (_, parent) = world.parent_from_env();
    let inherited = Some("conf dir/banto.toml");
    let invocations = invocations(&parent);
    for (label, argv) in [
        ("claude _mcp", &invocations.claude_mcp),
        ("codex _mcp", &invocations.codex_mcp),
        ("_wrap resume", &invocations.wrap_resume),
        ("_wrap new-session", &invocations.wrap_new_session),
    ] {
        let child = world
            .resolve_argv(argv, inherited)
            .unwrap_or_else(|err| panic!("{label} cannot start: {err:#}"));
        assert_agrees(label, &parent, &child, true);
    }
    let hook = world.resolve_hook(&invocations.hook_env, inherited);
    assert_agrees("_hook", &parent, &hook, false);
}

/// Paths whose names hold a double quote, an apostrophe, a space and a backslash
/// (a separator on Windows, an ordinary character elsewhere), as far as a
/// string-level test can take them: nothing here is created on disk. They go
/// through the real JSON args builder, the real Codex TOML override builder, the
/// `_wrap` builders and the hook's environment, then back through clap, and must
/// come out byte for byte.
#[test]
fn awkward_paths_come_through_every_builder_and_back_unchanged() {
    let world = World::new();
    let base = std::env::temp_dir();
    let awkward = |name: &str| base.join(format!("it's a \"quoted\" dir\\with sub {name}"));
    let parent = Resolved {
        config_file: ConfigFile::File(awkward("banto.toml")),
        db_path: Some(awkward("banto.db")),
        claude_home: Some(awkward("claude home")),
        codex_home: Some(awkward("codex home")),
        data_dir: Some(awkward("data dir")),
    };
    let invocations = invocations(&parent);

    // The paths really are in the arguments, whole.
    for args in [&invocations.claude_mcp, &invocations.codex_mcp] {
        for path in [
            awkward("banto.toml"),
            awkward("banto.db"),
            awkward("claude home"),
            awkward("codex home"),
        ] {
            let text = path.display().to_string();
            assert!(args.contains(&text), "{text:?} in {args:?}");
        }
    }
    // Nothing of the parent's own command line is copied beside them.
    for argv in [
        &invocations.claude_mcp,
        &invocations.codex_mcp,
        &invocations.wrap_resume,
        &invocations.wrap_new_session,
    ] {
        assert!(!argv.iter().any(|arg| arg == "--config"), "{argv:?}");
    }

    for (label, argv) in [
        ("claude _mcp", &invocations.claude_mcp),
        ("codex _mcp", &invocations.codex_mcp),
        ("_wrap resume", &invocations.wrap_resume),
        ("_wrap new-session", &invocations.wrap_new_session),
    ] {
        let child = world
            .resolve_argv(argv, None)
            .unwrap_or_else(|err| panic!("{label} cannot start: {err:#}"));
        assert_eq!(child, parent, "{label}");
    }
    let hook = world.resolve_hook(&invocations.hook_env, None);
    assert_agrees("_hook", &parent, &hook, false);
}
