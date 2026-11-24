//! When banto lists one home and the agent it opens will use another: which of
//! the flag, the config key, the variable and the default make the two part,
//! and what the notice says. Every location is inside a temp dir.

use super::resolution_tests::{Machine, resolved};
use super::*;

/// One product's way in through each source, so each case runs for both.
struct Product {
    label: &'static str,
    variable: &'static str,
    key: &'static str,
    dot_dir: &'static str,
    flag: fn(&mut Inputs, PathBuf),
    var: fn(&mut Inputs, &str),
}

const PRODUCTS: [Product; 2] = [
    Product {
        label: "Claude",
        variable: "CLAUDE_CONFIG_DIR",
        key: "claude_home",
        dot_dir: ".claude",
        flag: |inputs, path| inputs.cli_claude_home = Some(path),
        var: |inputs, value| inputs.env.claude_config_dir = Some(value.to_string()),
    },
    Product {
        label: "Codex",
        variable: "CODEX_HOME",
        key: "codex_home",
        dot_dir: ".codex",
        flag: |inputs, path| inputs.cli_codex_home = Some(path),
        var: |inputs, value| inputs.env.codex_home = Some(value.to_string()),
    },
];

fn spelled(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The notices for `inputs`, as `main` would carry them, for one product.
fn notices(inputs: &Inputs, product: &Product) -> Vec<String> {
    let (config, r) = resolve(inputs, Strictness::Strict).unwrap();
    agent_home_notices(inputs, &config, &r)
        .into_iter()
        .filter(|notice| notice.starts_with(&format!("{} home:", product.label)))
        .collect()
}

fn config_naming(m: &Machine, product: &Product, home: &Path) -> Inputs {
    m.write(
        "cwd/conf.toml",
        &format!("{} = '{}'\n", product.key, spelled(home)),
    );
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("conf.toml"));
    inputs
}

#[test]
fn the_default_and_a_variable_alone_agree_with_the_agent() {
    for product in &PRODUCTS {
        let m = Machine::new();
        assert!(
            notices(&m.inputs(), product).is_empty(),
            "{}",
            product.label
        );

        let mut inputs = m.inputs();
        (product.var)(&mut inputs, &spelled(&m.path("acct/work")));
        assert!(notices(&inputs, product).is_empty(), "{}", product.label);
    }
}

#[test]
fn an_empty_variable_is_the_unset_default_for_the_agent_too() {
    for product in &PRODUCTS {
        let m = Machine::new();
        let mut inputs = m.inputs();
        (product.var)(&mut inputs, "");
        assert!(notices(&inputs, product).is_empty(), "{}", product.label);
    }
}

#[test]
fn a_flag_that_names_the_variables_place_says_nothing_and_another_place_does() {
    for product in &PRODUCTS {
        let m = Machine::new();
        let work = m.path("acct/work");

        let mut inputs = m.inputs();
        (product.var)(&mut inputs, &spelled(&work));
        (product.flag)(&mut inputs, work.clone());
        assert!(notices(&inputs, product).is_empty(), "{}", product.label);

        let listed = m.path("elsewhere/home");
        (product.flag)(&mut inputs, listed.clone());
        let found = notices(&inputs, product);
        assert_eq!(
            found,
            [format!(
                "{label} home: banto lists {label} sessions under {}, but a {label} opened here \
                 uses {} (from {})",
                listed.display(),
                work.display(),
                product.variable,
                label = product.label
            )]
        );
    }
}

#[test]
fn a_flag_or_a_config_key_against_the_unset_default_names_the_default() {
    for product in &PRODUCTS {
        let m = Machine::new();
        let listed = m.path("elsewhere/home");
        let default = m.path("home").join(product.dot_dir);

        let mut by_flag = m.inputs();
        (product.flag)(&mut by_flag, listed.clone());
        let by_config = config_naming(&m, product, &listed);

        for inputs in [by_flag, by_config] {
            let found = notices(&inputs, product);
            assert_eq!(
                found,
                [format!(
                    "{label} home: banto lists {label} sessions under {}, but a {label} opened \
                     here uses {} (the default)",
                    listed.display(),
                    default.display(),
                    label = product.label
                )],
                "{}",
                product.label
            );
        }

        // The flag or the key may just as well name the default itself.
        let mut by_flag = m.inputs();
        (product.flag)(&mut by_flag, default.clone());
        assert!(notices(&by_flag, product).is_empty(), "{}", product.label);
        let by_config = config_naming(&m, product, &default);
        assert!(notices(&by_config, product).is_empty(), "{}", product.label);
    }
}

#[test]
fn a_config_key_against_the_variable_follows_the_same_rule() {
    for product in &PRODUCTS {
        let m = Machine::new();
        let work = m.path("acct/work");

        let mut agreeing = config_naming(&m, product, &work);
        (product.var)(&mut agreeing, &spelled(&work));
        assert!(notices(&agreeing, product).is_empty(), "{}", product.label);

        let mut parting = config_naming(&m, product, &m.path("elsewhere/home"));
        (product.var)(&mut parting, &spelled(&work));
        assert_eq!(notices(&parting, product).len(), 1, "{}", product.label);
    }
}

#[test]
fn the_flag_outranks_the_config_so_only_the_flag_is_compared() {
    for product in &PRODUCTS {
        let m = Machine::new();
        let work = m.path("acct/work");
        // The key disagrees with the variable, the flag overrides the key and agrees.
        let mut inputs = config_naming(&m, product, &m.path("elsewhere/home"));
        (product.var)(&mut inputs, &spelled(&work));
        (product.flag)(&mut inputs, work);
        assert!(notices(&inputs, product).is_empty(), "{}", product.label);
    }
}

#[test]
fn two_spellings_of_one_directory_agree() {
    for product in &PRODUCTS {
        let m = Machine::new();
        std::fs::create_dir_all(m.path("acct/work")).unwrap();
        let mut inputs = m.inputs();
        (product.var)(&mut inputs, &spelled(&m.path("acct/work")));
        (product.flag)(&mut inputs, m.path("acct/elsewhere/../work"));
        assert!(notices(&inputs, product).is_empty(), "{}", product.label);
    }
}

#[test]
fn a_relative_variable_is_reported_even_though_banto_made_it_absolute() {
    for product in &PRODUCTS {
        let m = Machine::new();
        let mut inputs = m.inputs();
        (product.var)(&mut inputs, "rel/home");
        // banto reads it against its own directory ...
        let listed = m.path("cwd").join("rel/home");
        assert_eq!(
            match product.label {
                "Claude" => resolved(&inputs).claude_home,
                _ => resolved(&inputs).codex_home,
            },
            Some(listed.clone())
        );
        // ... and says that the agent may not.
        assert_eq!(
            notices(&inputs, product),
            [format!(
                "{label} home: banto lists {label} sessions under {}, but {} is the relative \
                 path rel/home, which each pane's working directory can resolve differently",
                listed.display(),
                product.variable,
                label = product.label
            )]
        );

        // A flag that overrides it still leaves the agent with the relative variable.
        (product.flag)(&mut inputs, m.path("elsewhere/home"));
        assert_eq!(notices(&inputs, product).len(), 1, "{}", product.label);
    }
}

#[test]
fn no_home_to_list_or_no_home_for_the_agent_is_no_notice() {
    for product in &PRODUCTS {
        let m = Machine::new();
        let mut inputs = m.inputs();
        inputs.env.home = None;
        assert!(notices(&inputs, product).is_empty(), "{}", product.label);
    }
}

#[test]
fn only_an_enabled_product_is_looked_at() {
    let m = Machine::new();
    m.write("cwd/claude.toml", "agents = \"claude\"\n");
    m.write("cwd/codex.toml", "agents = \"codex\"\n");
    for (file, quiet, loud) in [
        ("claude.toml", &PRODUCTS[1], &PRODUCTS[0]),
        ("codex.toml", &PRODUCTS[0], &PRODUCTS[1]),
    ] {
        let mut inputs = m.inputs();
        inputs.cli_config = Some(PathBuf::from(file));
        for product in &PRODUCTS {
            (product.flag)(&mut inputs, m.path("elsewhere/home"));
        }
        assert!(notices(&inputs, quiet).is_empty(), "{file}");
        assert_eq!(notices(&inputs, loud).len(), 1, "{file}");
    }
}

#[test]
fn each_product_gets_its_own_notice_and_boot_notices_carry_them_after_the_database() {
    let m = Machine::new();
    m.write(
        "cwd/all.toml",
        "data_dir = \"data\"\ndb_path = \"elsewhere/banto.db\"\n",
    );
    let mut inputs = m.inputs();
    inputs.cli_config = Some(PathBuf::from("all.toml"));
    for product in &PRODUCTS {
        (product.flag)(&mut inputs, m.path("elsewhere/home"));
    }
    let (config, r) = resolve(&inputs, Strictness::Strict).unwrap();

    let homes = agent_home_notices(&inputs, &config, &r);
    assert_eq!(homes.len(), 2, "{homes:?}");
    assert!(homes[0].starts_with("Claude home:"), "{homes:?}");
    assert!(homes[1].starts_with("Codex home:"), "{homes:?}");

    let all = boot_notices(&inputs, &config, &r, Some("instance".to_string()));
    assert_eq!(all.len(), 4, "{all:?}");
    assert!(all[0].starts_with("data_dir:"), "{all:?}");
    assert_eq!(all[1..3], homes[..]);
    assert_eq!(all[3], "instance");

    // The ordinary launch, with nothing named, says nothing about homes.
    let plain = m.inputs();
    let (config, r) = resolve(&plain, Strictness::Strict).unwrap();
    assert!(boot_notices(&plain, &config, &r, None).is_empty());
}
