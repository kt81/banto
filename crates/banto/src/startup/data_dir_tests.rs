//! Where banto's data directory comes from — the flag a parent hands down,
//! `BANTO_DATA_DIR`, the config's `data_dir`, the platform — what is refused
//! (an empty value, anything inside an agent's home), and when the operator is
//! told the database does not live in it. Every location is inside a temp dir.

use super::resolution_tests::{Machine, resolved};
use super::*;

fn refusal(inputs: &Inputs) -> String {
    format!("{:#}", resolve(inputs, Strictness::Strict).unwrap_err())
}

// --- the four sources ------------------------------------------------------------

#[test]
fn the_data_directory_comes_from_the_flag_then_the_variable_then_the_config_then_the_platform() {
    let m = Machine::new();
    m.write("cwd/conf.toml", "data_dir = \"from-config\"\n");
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("conf.toml"));
    inputs.env.banto_data_dir = Some(PathBuf::from("from-env"));
    inputs.cli_data_dir = Some(m.path("from-flag"));
    assert_eq!(resolved(&inputs).data_dir, Some(m.path("from-flag")));

    inputs.cli_data_dir = None;
    // Relative means the directory banto started in, for the variable ...
    assert_eq!(resolved(&inputs).data_dir, Some(m.path("cwd/from-env")));

    inputs.env.banto_data_dir = None;
    // ... and for the config, as `db_path` does.
    assert_eq!(resolved(&inputs).data_dir, Some(m.path("cwd/from-config")));

    inputs.cli_config = None;
    assert_eq!(resolved(&inputs).data_dir, Some(m.path("platform")));
}

#[test]
fn the_default_database_follows_a_chosen_data_directory_and_an_explicit_one_does_not() {
    let m = Machine::new();
    m.write("cwd/conf.toml", "data_dir = \"chosen\"\n");
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("conf.toml"));
    let r = resolved(&inputs);
    assert_eq!(r.db_path, Some(m.path("cwd/chosen/banto.db")));
    assert_eq!(r.sweepable_data_dir().unwrap().root(), m.path("cwd/chosen"));

    m.write(
        "cwd/both.toml",
        "data_dir = \"chosen\"\ndb_path = \"x/y.db\"\n",
    );
    inputs.cli_config = Some(PathBuf::from("both.toml"));
    let r = resolved(&inputs);
    assert_eq!(r.data_dir, Some(m.path("cwd/chosen")));
    assert_eq!(r.db_path, Some(m.path("cwd/x/y.db")));
    assert_eq!(r.sweepable_data_dir(), None);
}

#[test]
fn an_empty_variable_is_no_variable() {
    let m = Machine::new();
    m.write("cwd/conf.toml", "data_dir = \"from-config\"\n");
    let mut inputs = m.inputs();
    inputs.env.banto_data_dir = Some(PathBuf::new());
    assert_eq!(resolved(&inputs).data_dir, Some(m.path("platform")));
    inputs.cli_config = Some(PathBuf::from("conf.toml"));
    assert_eq!(resolved(&inputs).data_dir, Some(m.path("cwd/from-config")));
}

#[test]
fn a_parents_hand_off_is_the_flag_alone() {
    let m = Machine::new();
    let parent = m.write("parent.toml", "data_dir = \"parents-config\"\n");
    // Handed over either way: a config file, or "no config".
    for (parent_config, parent_no_config) in [(Some(parent), false), (None, true)] {
        let mut inputs = m.inputs();
        inputs.parent_config = parent_config;
        inputs.parent_no_config = parent_no_config;
        inputs.env.banto_data_dir = Some(PathBuf::from("childs-env"));
        // What the parent resolved is what the child has: the parent had none, or
        // it would have passed it. Neither the child's variable nor the config the
        // parent loaded is a second opinion.
        assert_eq!(resolved(&inputs).data_dir, None);
        inputs.cli_data_dir = Some(m.path("parents flag"));
        assert_eq!(resolved(&inputs).data_dir, Some(m.path("parents flag")));
    }
}

// --- what is refused ------------------------------------------------------------

#[test]
fn an_empty_data_dir_in_the_config_is_refused_naming_the_key_and_the_file() {
    let m = Machine::new();
    m.write("cwd/conf.toml", "data_dir = \"\"\n");
    let file = m.path("cwd").join("conf.toml");
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("conf.toml"));
    let message = refusal(&inputs);
    assert!(message.contains("`data_dir`"), "{message}");
    assert!(message.contains(&file.display().to_string()), "{message}");
    assert!(message.contains("empty"), "{message}");
    assert!(message.contains("remove it"), "{message}");

    // The hook never fails the operator's own agent over banto's configuration:
    // it carries on, with no data directory it would not read anyway.
    let (config, r) = resolve(&inputs, Strictness::Lenient).unwrap();
    assert_eq!(config.data_dir, Some(PathBuf::new()));
    assert_eq!(r.data_dir, None);
}

#[test]
fn an_empty_data_dir_flag_is_refused() {
    let m = Machine::new();
    let mut inputs = m.inputs();
    inputs.cli_data_dir = Some(PathBuf::new());
    let message = refusal(&inputs);
    assert!(
        message.contains("`--data-dir`") && message.contains("empty"),
        "{message}"
    );
}

/// Each way of naming the directory, the message that names that way, and the
/// setup that makes the directory the one inside `.claude`.
#[test]
fn a_data_directory_inside_an_agent_home_is_refused_by_whatever_name_it_came() {
    let m = Machine::new();
    let claude = m.path("home/.claude");
    let codex = m.path("home/.codex");
    let config = m.write("cwd/conf.toml", "data_dir = \"placeholder\"\n");
    type Say = fn(&mut Inputs, &Path);
    let cases: [(&str, Say); 4] = [
        ("`--data-dir`", |inputs, dir| {
            inputs.cli_data_dir = Some(dir.to_path_buf());
        }),
        ("`BANTO_DATA_DIR`", |inputs, dir| {
            inputs.env.banto_data_dir = Some(dir.to_path_buf());
        }),
        ("the config key `data_dir`", |inputs, dir| {
            std::fs::write(
                inputs.cli_config.as_ref().unwrap(),
                format!("data_dir = {:?}\n", dir.display().to_string()),
            )
            .unwrap();
        }),
        ("platform", |inputs, dir| {
            // The config names no directory, so the platform's is the one in force.
            std::fs::write(inputs.cli_config.as_ref().unwrap(), "").unwrap();
            inputs.env.data_dir = Some(dir.to_path_buf());
        }),
    ];
    for (name, set) in cases {
        for (home_name, home) in [("Claude home", &claude), ("Codex home", &codex)] {
            for dir in [home.clone(), home.join("banto"), home.join("a/../banto/b")] {
                let mut inputs = m.inputs();
                inputs.cli_config = Some(config.clone());
                set(&mut inputs, &dir);
                let message = refusal(&inputs);
                assert!(message.contains(name), "{name}: {message}");
                assert!(message.contains(home_name), "{name}: {message}");
                assert!(
                    message.contains(&home.display().to_string()),
                    "{name}: {message}"
                );
            }
        }
    }
}

#[test]
fn a_directory_beside_or_above_an_agent_home_is_not_inside_it() {
    let m = Machine::new();
    for dir in [
        m.path("home/.claude-banto"),
        m.path("home/.claudeX/banto"),
        m.path("home/banto"),
        m.path("home"),
        m.path("home/.claude/../banto"),
    ] {
        let mut inputs = m.inputs();
        inputs.cli_data_dir = Some(dir.clone());
        assert_eq!(
            resolved(&inputs).data_dir,
            Some(dir.clone()),
            "{}",
            dir.display()
        );
    }
}

#[test]
fn the_homes_the_directory_is_checked_against_are_the_resolved_ones() {
    let m = Machine::new();
    let mut inputs = m.inputs();
    // A home moved by the config, the flag or the environment is the one that is
    // protected, not `~/.claude`.
    inputs.cli_claude_home = Some(m.path("elsewhere/claude"));
    inputs.cli_data_dir = Some(m.path("elsewhere/claude/banto"));
    assert!(refusal(&inputs).contains("Claude home"));
    inputs.cli_data_dir = Some(m.path("home/.claude/banto"));
    assert!(resolve(&inputs, Strictness::Strict).is_ok());

    let mut inputs = m.inputs();
    inputs.env.codex_home = Some("env-codex".to_string());
    inputs.cli_data_dir = Some(m.path("cwd/env-codex/banto"));
    assert!(refusal(&inputs).contains("Codex home"));
}

#[test]
fn a_database_inside_an_agent_home_is_refused_where_the_store_is_opened() {
    let m = Machine::new();
    for home in ["home/.claude", "home/.codex"] {
        for db in [m.path(home), m.path(home).join("banto.db")] {
            let mut inputs = m.inputs();
            inputs.cli_db_path = Some(db.clone());
            // Resolution carries on, as it does for an empty one (`list` opens no store) ...
            let r = resolved(&inputs);
            assert_eq!(r.db_path, Some(db.clone()));
            let before = files_under(&m.root);
            // ... and every way to the store refuses, before anything is created.
            let message = format!("{:#}", r.db_path().unwrap_err());
            assert!(message.contains("`db_path`"), "{message}");
            assert!(
                message.contains(&m.path(home).display().to_string()),
                "{message}"
            );
            assert!(crate::open_store(&r).is_err());
            assert_eq!(files_under(&m.root), before, "nothing was created");
        }
    }
    // Written in the config, the message says which file to look in.
    let file = m.write(
        "cwd/in-home.toml",
        &format!(
            "db_path = {:?}\n",
            m.path("home/.claude/x.db").display().to_string()
        ),
    );
    let mut inputs = m.inputs();
    inputs.cli_config = Some(file.clone());
    let message = format!("{:#}", resolved(&inputs).db_path().unwrap_err());
    assert!(message.contains(&file.display().to_string()), "{message}");
    // Beside the home is fine.
    inputs.cli_config = None;
    inputs.cli_db_path = Some(m.path("home/.claude-banto/x.db"));
    assert!(resolved(&inputs).db_path().is_ok());
}

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

// --- comparing paths --------------------------------------------------------------

#[test]
fn inside_means_the_same_normalised_path_or_below_it() {
    let inside = |path: &str, dir: &str| is_within(Path::new(path), Path::new(dir), Rules::EXACT);
    assert!(inside("/h/.claude", "/h/.claude"));
    assert!(inside("/h/.claude/banto", "/h/.claude"));
    assert!(inside("/h/.claude/banto/", "/h/.claude"));
    assert!(inside("/h/./.claude/x", "/h/.claude"));
    assert!(inside("/h/x/../.claude/y", "/h/.claude"));
    assert!(inside("/h/.claude/banto", "/h/.claude/"));
    assert!(!inside("/h/.claude-banto", "/h/.claude"));
    assert!(!inside("/h", "/h/.claude"));
    assert!(!inside("/h/.claude/../banto", "/h/.claude"));
    // `..` at the root stays at the root.
    assert!(inside("/../h/.claude/x", "/h/.claude"));
    // A relative path is not an absolute one, however alike they read.
    assert!(!inside("h/.claude/x", "/h/.claude"));
    // An empty directory contains nothing (an unresolved home must not refuse
    // every path).
    assert!(!inside("/h/x", ""));
}

#[test]
fn case_is_folded_only_when_the_rules_say_so() {
    let folded = Rules {
        fold_case: true,
        ..Rules::EXACT
    };
    let spelled = Path::new("/H/.CLAUDE/banto");
    let home = Path::new("/h/.claude");
    assert!(!is_within(spelled, home, Rules::EXACT));
    assert!(is_within(spelled, home, folded));
}

#[test]
fn a_platforms_rules_fold_case_where_its_default_volume_does() {
    // NTFS and APFS are case-insensitive by default. Folding on a volume that
    // is case-sensitive over-refuses, which is the safe direction.
    assert_eq!(
        Rules::of_this_platform().fold_case,
        cfg!(any(windows, target_os = "macos"))
    );
    assert_eq!(Rules::of_this_platform().win32_names, cfg!(windows));
}

#[test]
fn win32_names_lose_trailing_dots_and_spaces_and_a_stream_suffix() {
    let win32 = Rules {
        win32_names: true,
        ..Rules::EXACT
    };
    let home = Path::new("/h/.claude");
    for spelled in [
        "/h/.claude./banto",
        "/h/.claude../banto",
        "/h/.claude /banto",
        "/h/.claude. ./banto",
        "/h/.claude:x/banto",
        "/h/.claude::$INDEX_ALLOCATION/banto",
    ] {
        assert!(is_within(Path::new(spelled), home, win32), "{spelled}");
        assert!(
            !is_within(Path::new(spelled), home, Rules::EXACT),
            "{spelled}"
        );
    }
    assert!(!is_within(Path::new("/h/.claude.x/banto"), home, win32));
    assert!(!is_within(Path::new("/h/.claude-banto"), home, win32));
}

// --- the filesystem's own answer ---------------------------------------------------

/// What a test does when this machine cannot set up the thing it checks (a
/// directory link, an 8.3 name, an administrative share): it fails, because a
/// check that did not run must not look like one that passed. A machine known to
/// lack the facility opts out with `BANTO_TEST_ALLOW_SKIPS=1`, and the test then
/// says on stderr that it did not check.
fn cannot_check(what: &str) {
    assert!(
        std::env::var_os("BANTO_TEST_ALLOW_SKIPS").is_some_and(|value| value == "1"),
        "this machine cannot set up the check ({what}); set BANTO_TEST_ALLOW_SKIPS=1 to let \
         such a check go unmade"
    );
    eprintln!("skipped, not checked: {what}");
}

/// A link from `link` to the directory `target`, or `false` when this machine
/// would not make one (the caller then says so through [`cannot_check`]).
fn link_dir(link: &Path, target: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A junction needs no privilege, unlike a symlink.
        std::process::Command::new("cmd")
            .raw_arg(format!(
                "/C mklink /J \"{}\" \"{}\"",
                link.display(),
                target.display()
            ))
            .output()
            .is_ok_and(|out| out.status.success())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (link, target);
        false
    }
}

#[test]
fn a_link_into_an_agent_home_is_followed_so_it_does_not_hide_one() {
    let m = Machine::new();
    std::fs::create_dir_all(m.path("home/.claude")).unwrap();
    std::fs::create_dir_all(m.path("home/.codex")).unwrap();
    for (home_name, real, name) in [
        ("Claude home", "home/.claude", "link-to-claude"),
        ("Codex home", "home/.codex", "link-to-codex"),
    ] {
        let link = m.path(name);
        if !link_dir(&link, &m.path(real)) {
            return cannot_check("a directory link");
        }
        for dir in [link.clone(), link.join("banto"), link.join("not/yet/there")] {
            let mut inputs = m.inputs();
            inputs.cli_data_dir = Some(dir.clone());
            let message = refusal(&inputs);
            assert!(message.contains(home_name), "{}: {message}", dir.display());
            // The same door for the database.
            let mut inputs = m.inputs();
            inputs.cli_db_path = Some(dir.join("banto.db"));
            let message = format!("{:#}", resolved(&inputs).db_path().unwrap_err());
            assert!(message.contains(home_name), "{}: {message}", dir.display());
        }
    }
    // A link to somewhere else is no reason to refuse.
    std::fs::create_dir_all(m.path("elsewhere")).unwrap();
    let harmless = m.path("link-to-elsewhere");
    assert!(
        link_dir(&harmless, &m.path("elsewhere")),
        "a second directory link"
    );
    let mut inputs = m.inputs();
    inputs.cli_data_dir = Some(harmless.join("banto"));
    assert!(resolve(&inputs, Strictness::Strict).is_ok());
}

#[test]
fn a_home_that_is_itself_a_link_is_followed_too() {
    let m = Machine::new();
    std::fs::create_dir_all(m.path("real-claude")).unwrap();
    std::fs::create_dir_all(m.path("home")).unwrap();
    if !link_dir(&m.path("home/.claude"), &m.path("real-claude")) {
        return cannot_check("a directory link");
    }
    let mut inputs = m.inputs();
    inputs.cli_data_dir = Some(m.path("real-claude/banto"));
    assert!(refusal(&inputs).contains("Claude home"));
}

/// `home/.claude` spelled the Win32 way, with something trailing, then `banto`.
#[cfg(windows)]
fn trailing(m: &Machine, spelled: &str) -> PathBuf {
    PathBuf::from(format!("{}\\banto", m.path(spelled).display()))
}

#[cfg(windows)]
#[test]
fn on_windows_a_trailing_dot_or_space_does_not_hide_a_home_that_is_there() {
    let m = Machine::new();
    std::fs::create_dir_all(m.path("home/.claude")).unwrap();
    for spelled in ["home/.claude.", "home/.claude ", "home/.claude. ."] {
        let mut inputs = m.inputs();
        inputs.cli_data_dir = Some(trailing(&m, spelled));
        assert!(refusal(&inputs).contains("Claude home"), "{spelled:?}");
    }
}

#[cfg(windows)]
#[test]
fn on_windows_a_trailing_dot_or_space_does_not_hide_a_home_that_is_not_there() {
    let m = Machine::new();
    for spelled in ["home/.claude.", "home/.claude "] {
        let mut inputs = m.inputs();
        inputs.cli_data_dir = Some(trailing(&m, spelled));
        assert!(refusal(&inputs).contains("Claude home"), "{spelled:?}");
    }
}

/// The 8.3 alias of `path` (which exists), or `None` when the volume gives it
/// none or this machine would not say.
#[cfg(windows)]
fn short_name_of(path: &Path) -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("cmd")
        .raw_arg(format!(
            "/C for %I in (\"{}\") do @echo %~sI",
            path.display()
        ))
        .output()
        .ok()?;
    let short = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    (short.exists() && short != path && short.to_string_lossy().contains('~')).then_some(short)
}

#[cfg(windows)]
#[test]
fn on_windows_an_8_3_short_name_does_not_hide_a_home() {
    let m = Machine::new();
    // Longer than eight characters, so a volume that makes short names has one.
    let home = m.path("home/.claude");
    std::fs::create_dir_all(&home).unwrap();
    let Some(short) = short_name_of(&home) else {
        return cannot_check("an 8.3 name for the fake home (this volume makes none)");
    };
    for dir in [short.clone(), short.join("banto")] {
        let mut inputs = m.inputs();
        inputs.cli_data_dir = Some(dir.clone());
        let message = refusal(&inputs);
        assert!(
            message.contains("Claude home"),
            "{}: {message}",
            dir.display()
        );
        let mut inputs = m.inputs();
        inputs.cli_db_path = Some(dir.join("banto.db"));
        assert!(resolved(&inputs).db_path().is_err(), "{}", dir.display());
    }
}

// --- one directory, many names ------------------------------------------------------

/// `path`, which exists, as the local machine's administrative share names it,
/// in each spelling a reviewer found to get past a check by name: `localhost`,
/// the loopback address, the verbatim UNC form and the computer's own name.
/// `None` when the path is not on a drive or the computer's name is unknown.
#[cfg(windows)]
fn admin_share_spellings(path: &Path) -> Option<Vec<String>> {
    let real = path.canonicalize().ok()?;
    let text = real.to_string_lossy().into_owned();
    let (drive, rest) = text.strip_prefix(r"\\?\")?.split_once(r":\")?;
    let computer = std::env::var("COMPUTERNAME").ok()?;
    let mut spellings: Vec<String> = ["localhost", "127.0.0.1", computer.as_str()]
        .into_iter()
        .map(|host| format!(r"\\{host}\{drive}$\{rest}"))
        .collect();
    spellings.push(format!(r"\\?\UNC\localhost\{drive}$\{rest}"));
    Some(spellings)
}

#[cfg(windows)]
#[test]
fn on_windows_the_local_admin_share_does_not_hide_a_home() {
    let m = Machine::new();
    std::fs::create_dir_all(m.path("home/.claude")).unwrap();
    std::fs::create_dir_all(m.path("home/.claude-banto")).unwrap();
    let Some(homes) = admin_share_spellings(&m.path("home/.claude")) else {
        return cannot_check("the local admin share's name");
    };
    if homes.iter().any(|home| std::fs::metadata(home).is_err()) {
        return cannot_check("the local admin share (C$, D$ ...) is not reachable");
    }
    for home in homes {
        for dir in [
            home.clone(),
            format!(r"{home}\x"),
            format!(r"{home}\a\b\..\c"),
        ] {
            let mut inputs = m.inputs();
            inputs.cli_data_dir = Some(PathBuf::from(&dir));
            let message = refusal(&inputs);
            assert!(message.contains("Claude home"), "{dir}: {message}");
            // And the database, which is the other way in.
            let mut inputs = m.inputs();
            inputs.cli_db_path = Some(PathBuf::from(format!(r"{dir}\banto.db")));
            let message = format!("{:#}", resolved(&inputs).db_path().unwrap_err());
            assert!(message.contains("Claude home"), "{dir}: {message}");
        }
    }
    // A neighbour of the home by the same route is not the home.
    for neighbour in admin_share_spellings(&m.path("home/.claude-banto")).unwrap() {
        let mut inputs = m.inputs();
        inputs.cli_data_dir = Some(PathBuf::from(format!(r"{neighbour}\x")));
        assert!(
            resolve(&inputs, Strictness::Strict).is_ok(),
            "{neighbour} is not inside the home"
        );
    }
}

// --- when the working directory cannot be read -----------------------------------

#[test]
fn a_relative_data_directory_with_no_working_directory_is_refused_by_name() {
    let m = Machine::new();
    let mut inputs = m.inputs();
    inputs.cwd = None;
    inputs.cli_data_dir = Some(PathBuf::from("relative"));
    let message = refusal(&inputs);
    assert!(message.contains("`--data-dir`"), "{message}");
    assert!(message.contains("working directory"), "{message}");
    assert!(!message.contains("  "), "a stray run of spaces: {message}");
    // The hook goes without rather than fail the operator's agent.
    let (_, lenient) = resolve(&inputs, Strictness::Lenient).unwrap();
    assert_eq!(lenient.data_dir, None);
    // An absolute one needs no working directory.
    inputs.cli_data_dir = Some(m.path("absolute"));
    assert_eq!(resolved(&inputs).data_dir, Some(m.path("absolute")));
}

#[test]
fn a_relative_database_path_with_no_working_directory_is_refused_when_opened() {
    let m = Machine::new();
    let mut inputs = m.inputs();
    inputs.cwd = None;
    inputs.cli_db_path = Some(PathBuf::from("relative.db"));
    let message = format!("{:#}", resolved(&inputs).db_path().unwrap_err());
    assert!(message.contains("working directory"), "{message}");
    assert!(!message.contains("  "), "a stray run of spaces: {message}");
    inputs.cli_db_path = Some(m.path("absolute.db"));
    assert!(resolved(&inputs).db_path().is_ok());
}

#[cfg(windows)]
#[test]
fn on_windows_drive_letters_separators_case_and_the_verbatim_prefix_do_not_hide_a_home() {
    let rules = Rules::of_this_platform();
    let home = Path::new(r"C:\Users\Me\.claude");
    for spelled in [
        r"C:\Users\Me\.claude\banto",
        r"c:\users\me\.CLAUDE\banto",
        r"C:/Users/Me/.claude/banto",
        r"C:\Users\Me\sub\..\.claude\banto",
        r"\\?\C:\Users\Me\.claude\banto",
    ] {
        assert!(is_within(Path::new(spelled), home, rules), "{spelled}");
    }
    assert!(!is_within(
        Path::new(r"D:\Users\Me\.claude\banto"),
        home,
        rules
    ));
    assert!(!is_within(
        Path::new(r"C:\Users\Me\.claude-banto"),
        home,
        rules
    ));
}

#[cfg(windows)]
#[test]
fn on_windows_a_share_is_the_same_share_verbatim_or_not() {
    let rules = Rules::of_this_platform();
    let home = Path::new(r"\\Server\Share\Users\Me\.claude");
    for spelled in [
        r"\\server\share\users\me\.claude\banto",
        r"\\?\UNC\Server\Share\Users\Me\.claude\banto",
    ] {
        assert!(is_within(Path::new(spelled), home, rules), "{spelled}");
    }
    assert!(!is_within(
        Path::new(r"\\Server\Other\Users\Me\.claude\banto"),
        home,
        rules
    ));
}

// --- telling the operator the database lives elsewhere ---------------------------

#[test]
fn boot_notices_carry_the_database_then_the_instance_then_the_legacy_keys() {
    let m = Machine::new();
    m.write(
        "cwd/all.toml",
        "data_dir = \"data\"\ndb_path = \"elsewhere/banto.db\"\n\
         [brigade]\nworker_model_codex = \"terra\"\n",
    );
    m.write(
        "cwd/legacy-only.toml",
        "[brigade]\nworker_effort = \"low\"\n",
    );
    m.write(
        "cwd/new-spelling.toml",
        "[brigade.claude]\nworker_effort = \"low\"\n",
    );
    let notices = |file: &str, instance: Option<&str>| {
        let mut inputs = m.inputs();
        inputs.cli_config = Some(PathBuf::from(file));
        let (config, r) = resolve(&inputs, Strictness::Strict).unwrap();
        boot_notices(&inputs, &config, &r, instance.map(str::to_string))
    };

    let all = notices("all.toml", Some("instance"));
    assert_eq!(all.len(), 3, "{all:?}");
    assert!(
        all[0].starts_with("data_dir: the database is not in it"),
        "{all:?}"
    );
    assert_eq!(all[1], "instance");
    assert_eq!(
        all[2],
        "config: legacy [brigade] keys (worker_model_codex) moved to \
         [brigade.claude]/[brigade.codex]; see README"
    );

    let legacy_only = notices("legacy-only.toml", None);
    assert_eq!(legacy_only.len(), 1, "{legacy_only:?}");
    assert!(
        legacy_only[0].contains("(worker_effort)"),
        "{legacy_only:?}"
    );

    // The new spelling says nothing.
    assert!(notices("new-spelling.toml", None).is_empty());
}

fn notice_for(inputs: &Inputs) -> Option<String> {
    let (config, r) = resolve(inputs, Strictness::Strict).unwrap();
    split_database_notice(inputs, &config, &r)
}

#[test]
fn a_notice_only_when_the_data_directory_and_the_database_are_both_named_and_apart() {
    let m = Machine::new();
    m.write(
        "cwd/apart.toml",
        "data_dir = \"data\"\ndb_path = \"elsewhere/banto.db\"\n",
    );
    m.write(
        "cwd/together.toml",
        "data_dir = \"data\"\ndb_path = \"data/banto.db\"\n",
    );
    m.write("cwd/dir-only.toml", "data_dir = \"data\"\n");
    m.write("cwd/db-only.toml", "db_path = \"elsewhere/banto.db\"\n");

    let with = |file: &str| {
        let mut inputs = m.inputs();
        inputs.cli_config = Some(PathBuf::from(file));
        inputs
    };
    let notice = notice_for(&with("apart.toml")).expect("both named and apart");
    for word in ["data_dir", "database", "sweep", "MCP"] {
        assert!(notice.contains(word), "{word}: {notice}");
    }
    assert!(!notice.contains('\n'));

    assert_eq!(notice_for(&with("together.toml")), None);
    assert_eq!(notice_for(&with("dir-only.toml")), None);
    // `db_path` alone is the long-standing way to point at another database: silent.
    assert_eq!(notice_for(&with("db-only.toml")), None);
    assert_eq!(notice_for(&m.inputs()), None);
}

#[test]
fn the_notice_follows_every_way_of_naming_the_directory_but_not_the_platform_default() {
    let m = Machine::new();
    m.write("cwd/db.toml", "db_path = \"elsewhere/banto.db\"\n");
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("db.toml"));
    // The platform's own directory, with a database elsewhere: as today, silent.
    assert_eq!(notice_for(&inputs), None);

    let mut by_variable = inputs.clone();
    by_variable.env.banto_data_dir = Some(PathBuf::from("data"));
    assert!(notice_for(&by_variable).is_some());

    let mut by_flag = inputs.clone();
    by_flag.cli_data_dir = Some(m.path("data"));
    assert!(notice_for(&by_flag).is_some());

    // The platform directory named explicitly is still a choice.
    let mut by_name = inputs.clone();
    by_name.env.banto_data_dir = Some(m.path("platform"));
    assert!(notice_for(&by_name).is_some());

    // An empty variable names nothing.
    let mut empty = inputs;
    empty.env.banto_data_dir = Some(PathBuf::new());
    assert_eq!(notice_for(&empty), None);
}

// --- the children agree ---------------------------------------------------------

#[test]
fn a_child_is_handed_the_directory_the_parent_chose_by_whatever_means() {
    let m = Machine::new();
    let mut inputs = m.inputs();
    inputs.env.banto_data_dir = Some(PathBuf::from("chosen"));
    let parent = resolved(&inputs);
    assert_eq!(parent.data_dir, Some(m.path("cwd/chosen")));
    let args = parent.child_args();
    let at = args.iter().position(|arg| arg == "--data-dir").unwrap();
    assert_eq!(
        args[at + 1],
        m.path("cwd").join("chosen").display().to_string()
    );
    // The hook is handed it too, through the environment: it reads a Goinkyo's
    // consultation request from there.
    let dir = m.path("cwd").join("chosen").display().to_string();
    assert!(
        parent
            .child_env()
            .contains(&(ENV_PARENT_DATA_DIR.to_string(), dir)),
        "{:?}",
        parent.child_env()
    );
}
