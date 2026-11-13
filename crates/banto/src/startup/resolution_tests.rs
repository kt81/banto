//! The resolution itself: what wins over what, what becomes absolute, and what
//! a parent's hand-off does and does not change. Every location is inside a
//! temp dir.

use clap::Parser;

use super::*;
use crate::Cli;

struct Machine {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        Self {
            root: tmp.path().to_path_buf(),
            _tmp: tmp,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn env(&self) -> Env {
        Env {
            home: Some(self.path("home")),
            default_config: Some(self.path("platform/config.toml")),
            default_db: Some(self.path("platform/banto.db")),
            ..Env::default()
        }
    }

    fn inputs(&self) -> Inputs {
        Inputs {
            env: self.env(),
            cwd: Some(self.path("cwd")),
            ..Inputs::default()
        }
    }
}

fn resolved(inputs: &Inputs) -> Resolved {
    resolve(inputs, Strictness::Strict).unwrap().1
}

#[test]
fn the_config_search_order_is_flag_then_environment_then_xdg_then_home_then_the_platform() {
    let m = Machine::new();
    let flag = m.write("flag.toml", "");
    let env = m.write("env.toml", "");
    m.write("xdg/banto/config.toml", "");
    m.write("home/.config/banto/config.toml", "");

    let mut inputs = m.inputs();
    inputs.env.xdg_config_home = Some(m.path("xdg"));
    inputs.env.banto_config = Some(env.clone());
    inputs.cli_config = Some(flag.clone());
    assert_eq!(resolved(&inputs).config_file, ConfigFile::File(flag));

    inputs.cli_config = None;
    assert_eq!(resolved(&inputs).config_file, ConfigFile::File(env));

    inputs.env.banto_config = None;
    assert_eq!(
        resolved(&inputs).config_file,
        ConfigFile::File(m.path("xdg/banto/config.toml"))
    );

    inputs.env.xdg_config_home = None;
    assert_eq!(
        resolved(&inputs).config_file,
        ConfigFile::File(m.path("home/.config/banto/config.toml"))
    );

    std::fs::remove_file(m.path("home/.config/banto/config.toml")).unwrap();
    assert_eq!(
        resolved(&inputs).config_file,
        ConfigFile::File(m.path("platform/config.toml"))
    );

    inputs.env.default_config = None;
    assert_eq!(resolved(&inputs).config_file, ConfigFile::None);
}

#[test]
fn an_explicit_config_that_is_missing_is_an_error_unless_the_resolution_is_lenient() {
    let m = Machine::new();
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("no/such.toml"));
    assert!(resolve(&inputs, Strictness::Strict).is_err());
    let (config, resolved) = resolve(&inputs, Strictness::Lenient).unwrap();
    assert_eq!(config, Config::default());
    assert_eq!(
        resolved.config_file,
        ConfigFile::File(m.path("cwd/no/such.toml"))
    );
}

#[test]
fn relative_inputs_mean_the_directory_the_process_started_in() {
    let m = Machine::new();
    m.write(
        "cwd/conf.toml",
        "db_path = \"data/banto.db\"\nclaude_home = \"c\"\ncodex_home = \"x\"\n",
    );
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("conf.toml"));
    let r = resolved(&inputs);
    assert_eq!(r.config_file, ConfigFile::File(m.path("cwd/conf.toml")));
    assert_eq!(r.db_path, Some(m.path("cwd/data/banto.db")));
    assert_eq!(r.claude_home, Some(m.path("cwd/c")));
    assert_eq!(r.codex_home, Some(m.path("cwd/x")));

    // An absolute value is left alone.
    inputs.cli_claude_home = Some(m.path("elsewhere"));
    assert_eq!(resolved(&inputs).claude_home, Some(m.path("elsewhere")));
}

#[test]
fn homes_database_and_config_each_keep_their_priority_order() {
    let m = Machine::new();
    m.write(
        "cwd/conf.toml",
        "db_path = \"from-config.db\"\nclaude_home = \"cfg-claude\"\n",
    );
    let mut inputs = m.inputs();
    inputs.env.claude_config_dir = Some("env-claude".to_string());
    inputs.env.codex_home = Some("env-codex".to_string());

    // Nothing in the config: the environment's variables, then the home's dot-dir.
    let r = resolved(&inputs);
    assert_eq!(r.claude_home, Some(m.path("cwd/env-claude")));
    assert_eq!(r.codex_home, Some(m.path("cwd/env-codex")));
    assert_eq!(r.db_path, Some(m.path("platform/banto.db")));
    inputs.env.claude_config_dir = None;
    inputs.env.codex_home = None;
    let r = resolved(&inputs);
    assert_eq!(r.claude_home, Some(m.path("home/.claude")));
    assert_eq!(r.codex_home, Some(m.path("home/.codex")));

    // The config beats the environment; a flag beats the config.
    inputs.env.claude_config_dir = Some("env-claude".to_string());
    inputs.cli_config = Some(PathBuf::from("conf.toml"));
    let r = resolved(&inputs);
    assert_eq!(r.claude_home, Some(m.path("cwd/cfg-claude")));
    assert_eq!(r.db_path, Some(m.path("cwd/from-config.db")));
    inputs.cli_claude_home = Some(m.path("flag-claude"));
    inputs.cli_db_path = Some(m.path("flag.db"));
    let r = resolved(&inputs);
    assert_eq!(r.claude_home, Some(m.path("flag-claude")));
    assert_eq!(r.db_path, Some(m.path("flag.db")));
}

#[test]
fn what_a_parent_handed_over_beats_the_search_and_the_environment() {
    let m = Machine::new();
    let parents = m.write("parent.toml", "db_path = \"parent.db\"\n");
    m.write("env.toml", "db_path = \"env.db\"\n");
    let mut inputs = m.inputs();
    inputs.env.banto_config = Some(m.path("env.toml"));
    inputs.parent_config = Some(parents.clone());
    let r = resolved(&inputs);
    assert_eq!(r.config_file, ConfigFile::File(parents));
    assert_eq!(r.db_path, Some(m.path("cwd/parent.db")));

    // "No config" is as final: the environment's file is not consulted.
    inputs.parent_config = None;
    inputs.parent_no_config = true;
    let (config, r) = resolve(&inputs, Strictness::Strict).unwrap();
    assert_eq!(r.config_file, ConfigFile::None);
    assert_eq!(config, Config::default());
    assert_eq!(r.db_path, Some(m.path("platform/banto.db")));
}

#[test]
fn a_parents_config_is_loaded_as_the_parent_loaded_it_even_when_it_does_not_parse() {
    // The parent started with it (a discovered file that is broken is the
    // defaults, not a failure); the child must not fail where the parent did not.
    let m = Machine::new();
    let broken = m.write("broken.toml", "this is not toml [");
    let mut inputs = m.inputs();
    inputs.parent_config = Some(broken.clone());
    let (config, r) = resolve(&inputs, Strictness::Strict).unwrap();
    assert_eq!(config, Config::default());
    assert_eq!(r.config_file, ConfigFile::File(broken));
}

#[test]
fn a_child_started_without_the_hand_off_resolves_for_itself_as_before() {
    let m = Machine::new();
    // A hand-run `banto _mcp`, or an MCP config an older banto wrote: no flags.
    let cli =
        Cli::try_parse_from(["banto", "_mcp", "--brigade", "1", "--member", "worker-1"]).unwrap();
    assert!(cli.parent_config.is_none() && !cli.parent_no_config && cli.db_path.is_none());
    let r = resolved(&m.inputs());
    assert_eq!(
        r.config_file,
        ConfigFile::File(m.path("platform/config.toml"))
    );
    assert_eq!(r.db_path, Some(m.path("platform/banto.db")));
}

#[test]
fn a_hook_without_the_variables_resolves_for_itself_and_empty_ones_change_nothing() {
    let m = Machine::new();
    let mut inputs = m.inputs();
    // Only the identity banto always set.
    inputs.apply_parent_env(|key| match key {
        "BANTO_BRIGADE" => Some("7".to_string()),
        "BANTO_MEMBER" => Some("worker-1".to_string()),
        _ => None,
    });
    assert!(
        inputs.parent_config.is_none() && !inputs.parent_no_config && inputs.cli_db_path.is_none()
    );
    let by_itself = resolved(&inputs);
    assert_eq!(
        by_itself.config_file,
        ConfigFile::File(m.path("platform/config.toml"))
    );

    inputs.apply_parent_env(|_| Some(String::new()));
    assert!(
        inputs.parent_config.is_none() && !inputs.parent_no_config && inputs.cli_db_path.is_none()
    );
    assert_eq!(resolved(&inputs), by_itself);
}

/// What `Resolved::child_args` says, taken back through the real command line.
fn through_the_command_line(r: &Resolved, cwd: &Path, subcommand: &str) -> Resolved {
    let mut argv = vec!["banto".to_string(), subcommand.to_string()];
    argv.extend(r.child_args());
    let cli = Cli::try_parse_from(&argv).unwrap_or_else(|err| panic!("{err}\n{argv:?}"));
    let m = Machine::new();
    let inputs = Inputs {
        cli_config: cli.config,
        cli_claude_home: cli.claude_home,
        cli_codex_home: cli.codex_home,
        cli_db_path: cli.db_path,
        parent_config: cli.parent_config,
        parent_no_config: cli.parent_no_config,
        env: m.env(),
        cwd: Some(cwd.to_path_buf()),
    };
    resolved(&inputs)
}

#[test]
fn child_args_survive_the_command_line_for_paths_with_spaces_backslashes_and_quotes() {
    // Plain strings, not real paths: what matters is that each is exactly one
    // argument and comes back byte for byte, on any platform.
    for tricky in [
        "/home/me/my config/banto.toml",
        r"C:\Users\me\My Config\banto.toml",
        r#"/odd/"quoted" dir/it's here/banto.toml"#,
        "/trailing-space /x.toml",
    ] {
        let r = Resolved {
            config_file: ConfigFile::File(PathBuf::from(tricky)),
            db_path: Some(PathBuf::from(format!("{tricky}.db"))),
            claude_home: Some(PathBuf::from(format!("{tricky}-claude"))),
            codex_home: Some(PathBuf::from(format!("{tricky}-codex"))),
        };
        let args = r.child_args();
        assert!(args.contains(&tricky.to_string()), "{args:?}");
        assert!(args.iter().all(|arg| !arg.is_empty()));
        // Absolute only on its own platform, so compare what came back to what
        // went in only where the string is absolute here; elsewhere the point
        // is the single-argument encoding above.
        if Path::new(tricky).is_absolute() {
            let back = through_the_command_line(&r, Path::new("/elsewhere"), "_mcp");
            assert_eq!(back, r, "{tricky}");
        }
    }
}

#[test]
fn child_args_and_env_say_the_same_two_things_about_the_config_and_the_database() {
    let with_file = Resolved {
        config_file: ConfigFile::File(PathBuf::from("/c/banto.toml")),
        db_path: Some(PathBuf::from("/d/banto.db")),
        claude_home: Some(PathBuf::from("/h/claude")),
        codex_home: None,
    };
    assert_eq!(
        with_file.child_args(),
        [
            "--parent-config",
            "/c/banto.toml",
            "--db-path",
            "/d/banto.db",
            "--claude-home",
            "/h/claude"
        ]
    );
    assert_eq!(
        with_file.child_env(),
        [
            (ENV_PARENT_CONFIG.to_string(), "/c/banto.toml".to_string()),
            (ENV_PARENT_DB.to_string(), "/d/banto.db".to_string())
        ]
    );

    let without = Resolved {
        config_file: ConfigFile::None,
        db_path: None,
        claude_home: None,
        codex_home: None,
    };
    assert_eq!(without.child_args(), ["--parent-no-config"]);
    assert_eq!(
        without.child_env(),
        [(ENV_PARENT_NO_CONFIG.to_string(), "1".to_string())]
    );
}

#[test]
fn the_hand_off_never_touches_an_agents_own_variables() {
    // Where an agent keeps its home is the agent's business, not banto's.
    let r = Resolved {
        config_file: ConfigFile::File(PathBuf::from("/c.toml")),
        db_path: Some(PathBuf::from("/d.db")),
        claude_home: Some(PathBuf::from("/h/claude")),
        codex_home: Some(PathBuf::from("/h/codex")),
    };
    for (name, _) in r.child_env() {
        assert!(name.starts_with("BANTO_"), "{name}");
        assert!(name != "CLAUDE_CONFIG_DIR" && name != "CODEX_HOME");
    }
}

#[test]
fn the_hidden_flags_are_accepted_anywhere_after_the_subcommand_and_exclude_each_other() {
    let cli = Cli::try_parse_from([
        "banto",
        "_wrap",
        "--parent-config",
        "/c.toml",
        "--session",
        "s",
        "--db-path",
        "/d.db",
        "--",
        "claude",
    ])
    .unwrap();
    assert_eq!(cli.parent_config, Some(PathBuf::from("/c.toml")));
    assert_eq!(cli.db_path, Some(PathBuf::from("/d.db")));
    assert!(
        Cli::try_parse_from([
            "banto",
            "_mcp",
            "--parent-config",
            "/c.toml",
            "--parent-no-config"
        ])
        .is_err()
    );
}

// --- an empty path is not the working directory ---------------------------------

#[test]
fn an_empty_database_path_stays_empty_as_it_always_reached_the_store() {
    let m = Machine::new();
    m.write("cwd/conf.toml", "db_path = \"\"\n");
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("conf.toml"));
    assert_eq!(resolved(&inputs).db_path, Some(PathBuf::new()));
    inputs.cli_config = None;
    inputs.cli_db_path = Some(PathBuf::new());
    assert_eq!(resolved(&inputs).db_path, Some(PathBuf::new()));
}

#[test]
fn an_empty_config_path_reaches_the_strict_loader_empty_from_the_flag_and_the_environment() {
    let m = Machine::new();
    for from_flag in [true, false] {
        let mut inputs = m.inputs();
        if from_flag {
            inputs.cli_config = Some(PathBuf::new());
        } else {
            inputs.env.banto_config = Some(PathBuf::new());
        }
        let error = resolve(&inputs, Strictness::Strict).unwrap_err();
        let chain = format!("{error:#}");
        // The loader was handed the empty path itself, not the working directory.
        assert!(
            chain.starts_with("failed to load config file : config file not found"),
            "{chain}"
        );
        // Leniently it is the defaults, still naming the empty path.
        let (config, resolved) = resolve(&inputs, Strictness::Lenient).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(resolved.config_file, ConfigFile::File(PathBuf::new()));
    }
}

#[test]
fn an_empty_home_stays_empty_too() {
    let m = Machine::new();
    let mut inputs = m.inputs();
    inputs.cli_claude_home = Some(PathBuf::new());
    inputs.cli_codex_home = Some(PathBuf::new());
    let r = resolved(&inputs);
    assert_eq!(r.claude_home, Some(PathBuf::new()));
    assert_eq!(r.codex_home, Some(PathBuf::new()));
}

// --- no working directory --------------------------------------------------------

#[test]
fn with_no_readable_working_directory_nothing_fails_and_relative_paths_stay_as_given() {
    let m = Machine::new();
    let config = m.write("conf.toml", "db_path = \"state/banto.db\"\n");
    let mut inputs = m.inputs();
    inputs.cwd = None;
    inputs.cli_config = Some(config.clone());
    inputs.cli_claude_home = Some(PathBuf::from("rel/claude"));
    let r = resolved(&inputs);
    assert_eq!(r.config_file, ConfigFile::File(config));
    // Relative values are what HEAD used: raw.
    assert_eq!(r.db_path, Some(PathBuf::from("state/banto.db")));
    assert_eq!(r.claude_home, Some(PathBuf::from("rel/claude")));

    // Nothing relative, nothing to fail over.
    let mut plain = m.inputs();
    plain.cwd = None;
    assert!(resolve(&plain, Strictness::Strict).is_ok());
}

#[test]
fn the_hooks_lenient_resolution_also_copes_without_a_working_directory() {
    let m = Machine::new();
    let mut inputs = m.inputs();
    inputs.cwd = None;
    inputs.env.banto_config = Some(PathBuf::from("relative/banto.toml"));
    let (config, r) = resolve(&inputs, Strictness::Lenient).unwrap();
    assert_eq!(config, Config::default());
    assert_eq!(
        r.config_file,
        ConfigFile::File(PathBuf::from("relative/banto.toml"))
    );
    assert_eq!(r.db_path, Some(m.path("platform/banto.db")));
}

// --- what the parent may lack ----------------------------------------------------

#[test]
fn a_child_handed_the_parents_answers_has_no_codex_home_when_the_parent_had_none() {
    let m = Machine::new();
    // The child's own environment could produce a Codex home ...
    let mut inputs = m.inputs();
    inputs.env.codex_home = Some("env-codex".to_string());
    assert!(resolved(&inputs).codex_home.is_some());

    // ... but a child carrying the hand-off, with no `--codex-home`, takes that as
    // "the parent had none" — in either form of the hand-off.
    inputs.parent_no_config = true;
    assert_eq!(resolved(&inputs).codex_home, None);
    inputs.parent_no_config = false;
    inputs.parent_config = Some(m.write("parent.toml", "codex_home = \"cfg-codex\"\n"));
    assert_eq!(resolved(&inputs).codex_home, None);

    // An explicit `--codex-home` is always honoured.
    inputs.cli_codex_home = Some(m.path("given"));
    assert_eq!(resolved(&inputs).codex_home, Some(m.path("given")));
}

#[test]
fn a_parent_without_a_codex_home_hands_its_child_none_end_to_end() {
    let m = Machine::new();
    let parent = Resolved {
        config_file: ConfigFile::None,
        db_path: Some(m.path("d.db")),
        claude_home: Some(m.path("claude")),
        codex_home: None,
    };
    assert!(!parent.child_args().iter().any(|arg| arg == "--codex-home"));
    // The child's environment would give it one; it does not take it.
    let mut env = m.env();
    env.codex_home = Some("env-codex".to_string());
    let mut argv = vec!["banto".to_string(), "_mcp".to_string()];
    argv.extend(parent.child_args());
    let cli = Cli::try_parse_from(&argv).unwrap();
    let inputs = Inputs {
        cli_config: cli.config,
        cli_claude_home: cli.claude_home,
        cli_codex_home: cli.codex_home,
        cli_db_path: cli.db_path,
        parent_config: cli.parent_config,
        parent_no_config: cli.parent_no_config,
        env,
        cwd: Some(m.path("session")),
    };
    assert_eq!(resolved(&inputs), parent);
}

#[test]
fn a_parent_that_starts_children_always_has_a_claude_home_and_a_database() {
    // Both children-launching modes open the store and resolve the Claude home
    // before they can start anything (`main`'s default arm, `tui::run`,
    // `embedded::run_emporium`): a parent without either fails first, so no child
    // ever meets their absence. Pinned here as the resolution's own contract: the
    // two are errors, not silent gaps, when missing.
    let m = Machine::new();
    let mut inputs = m.inputs();
    inputs.env.home = None;
    inputs.env.default_db = None;
    let r = resolved(&inputs);
    assert!(r.claude_home().is_err());
    assert!(r.db_path().is_err());
}

// --- the platform's own locations are made absolute like everything else --------

#[test]
fn the_platform_defaults_and_the_home_derived_ones_are_made_absolute_too() {
    let m = Machine::new();
    m.write("cwd/plat/config.toml", "db_path = \"from-platform.db\"\n");
    let mut inputs = m.inputs();
    inputs.env.default_config = Some(PathBuf::from("plat/config.toml"));
    inputs.env.default_db = Some(PathBuf::from("plat/banto.db"));
    inputs.env.home = Some(PathBuf::from("rel home"));
    let r = resolved(&inputs);
    // The relative platform config location is the file that was found and read.
    assert_eq!(
        r.config_file,
        ConfigFile::File(m.path("cwd/plat/config.toml"))
    );
    assert_eq!(r.db_path, Some(m.path("cwd/from-platform.db")));
    assert_eq!(r.claude_home, Some(m.path("cwd/rel home/.claude")));
    assert_eq!(r.codex_home, Some(m.path("cwd/rel home/.codex")));
    inputs.env.default_config = Some(PathBuf::from("plat/none.toml"));
    assert_eq!(resolved(&inputs).db_path, Some(m.path("cwd/plat/banto.db")));
    // Absolute ones come out exactly as they went in.
    let mut absolute_inputs = m.inputs();
    absolute_inputs.env.home = Some(m.path("abs home"));
    assert_eq!(
        resolved(&absolute_inputs).claude_home,
        Some(m.path("abs home/.claude"))
    );
}

// --- flags that must not be combined --------------------------------------------

#[test]
fn a_config_flag_next_to_either_parent_flag_is_rejected_wherever_each_is_placed() {
    for parent_flag in [
        vec!["--parent-config", "/c.toml"],
        vec!["--parent-no-config"],
    ] {
        // Both after the subcommand name: clap itself refuses.
        let mut argv = vec!["banto", "_mcp", "--config", "/mine.toml"];
        argv.extend(parent_flag.clone());
        assert!(Cli::try_parse_from(&argv).is_err(), "{argv:?}");
        // One before and one after: parsed apart by clap, refused by the check
        // `main` runs on the result.
        let mut argv = vec!["banto", "--config", "/mine.toml", "_wrap"];
        argv.extend(parent_flag.clone());
        argv.extend(["--session", "s", "--", "claude"]);
        let cli = Cli::try_parse_from(&argv).expect("clap alone does not see it");
        assert!(cli.check_hand_off_flags().is_err(), "{argv:?}");
    }
    // Neither alone is a conflict.
    let cli = Cli::try_parse_from(["banto", "--config", "/mine.toml", "list"]).unwrap();
    assert!(cli.check_hand_off_flags().is_ok());
    let cli = Cli::try_parse_from(["banto", "_mcp", "--parent-no-config"]).unwrap();
    assert!(cli.check_hand_off_flags().is_ok());
}

#[test]
fn a_childs_arguments_never_carry_the_parents_own_config_flag() {
    // The hand-off is built from what was resolved, never copied from the parent's
    // own command line, so `--config` cannot appear beside `--parent-config`.
    for config_file in [ConfigFile::File(PathBuf::from("/c.toml")), ConfigFile::None] {
        let r = Resolved {
            config_file,
            db_path: Some(PathBuf::from("/d.db")),
            claude_home: Some(PathBuf::from("/h")),
            codex_home: Some(PathBuf::from("/x")),
        };
        assert!(!r.child_args().iter().any(|arg| arg == "--config"));
    }
}

#[test]
fn a_hook_given_both_config_variables_resolves_to_no_config() {
    let m = Machine::new();
    let mut inputs = m.inputs();
    inputs.apply_parent_env(|key| match key {
        ENV_PARENT_CONFIG => Some(m.path("c.toml").display().to_string()),
        ENV_PARENT_NO_CONFIG => Some("1".to_string()),
        _ => None,
    });
    assert_eq!(resolved(&inputs).config_file, ConfigFile::None);
}

// --- an empty database path is a startup error ----------------------------------

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            found.push(path);
        }
    }
    found.sort();
    found
}

#[test]
fn an_empty_db_path_is_refused_naming_the_key_and_the_config_file_and_nothing_is_created() {
    let m = Machine::new();
    m.write("cwd/conf.toml", "db_path = \"\"\n");
    let config = m.path("cwd").join("conf.toml");
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("conf.toml"));
    let r = resolved(&inputs);
    // Resolution itself is pure and carries on (`list` and `versions` open no store) ...
    assert_eq!(r.db_path, Some(PathBuf::new()));

    let before = files_under(&m.root);
    // ... but every path to the store refuses, before anything is opened.
    let message = format!("{:#}", r.db_path().unwrap_err());
    assert!(message.contains("`db_path`"), "{message}");
    assert!(message.contains(&config.display().to_string()), "{message}");
    assert!(message.contains("remove the key"), "{message}");
    let opened = crate::open_store(&r);
    assert!(opened.is_err());
    assert_eq!(files_under(&m.root), before, "nothing was created");
}

#[test]
fn an_empty_database_path_handed_to_a_child_is_refused_the_same_way() {
    let m = Machine::new();
    let mut inputs = m.inputs();
    inputs.parent_no_config = true;
    inputs.cli_db_path = Some(PathBuf::new());
    let r = resolved(&inputs);
    let message = format!("{:#}", r.db_path().unwrap_err());
    assert!(
        message.contains("`db_path`") && message.contains("empty"),
        "{message}"
    );
    assert!(crate::open_store(&r).is_err());
}

#[test]
fn a_whitespace_only_database_path_is_a_file_name_like_every_other_path_key() {
    // The config loader takes path keys as written (a `PathBuf`, no trimming), so
    // only the empty path is refused.
    let m = Machine::new();
    m.write("cwd/conf.toml", "db_path = \"  \"\n");
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("conf.toml"));
    let r = resolved(&inputs);
    assert_eq!(r.db_path, Some(m.path("cwd/  ")));
    assert!(r.db_path().is_ok());
}

#[test]
fn a_valid_relative_or_absolute_db_path_is_unaffected() {
    let m = Machine::new();
    m.write("cwd/conf.toml", "db_path = \"state/banto.db\"\n");
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("conf.toml"));
    let r = resolved(&inputs);
    assert_eq!(r.db_path().unwrap(), m.path("cwd/state/banto.db"));
    inputs.cli_db_path = Some(m.path("elsewhere/x.db"));
    assert_eq!(
        resolved(&inputs).db_path().unwrap(),
        m.path("elsewhere/x.db")
    );
    // And the default location, when nothing names one.
    assert_eq!(
        resolved(&m.inputs()).db_path().unwrap(),
        m.path("platform/banto.db")
    );
}
