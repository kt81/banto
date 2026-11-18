//! What `banto --version` says: the package version, the commit it was built
//! from, and the database schema it migrates to.
//!
//! The schema is there because it is the part of a banto that outlives the
//! binary: a database a newer build has migrated cannot be opened by an older
//! one (`banto_io::store::StoreError::UnsupportedSchemaVersion`), and the
//! operator choosing between two builds on disk needs to see which numbers they
//! carry. The commit comes from the build environment, not from `build.rs`: the
//! release workflow sets `BANTO_BUILD_COMMIT` for the compiler, and a build made
//! without it (`cargo build`, `cargo install --git`) says `commit unknown`
//! rather than guessing from a checkout it may not be in.

use std::sync::OnceLock;

/// The text clap prints after the program's name.
pub fn version() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        describe(
            env!("CARGO_PKG_VERSION"),
            option_env!("BANTO_BUILD_COMMIT"),
            banto_io::store::SCHEMA_VERSION,
        )
    })
}

/// `0.1.0 (commit abc1234, schema 18)`. The commit is printed as given; a missing
/// or blank one is `unknown`.
fn describe(package: &str, commit: Option<&str>, schema: i64) -> String {
    let commit = commit.map(str::trim).filter(|commit| !commit.is_empty());
    format!(
        "{package} (commit {}, schema {schema})",
        commit.unwrap_or("unknown")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_carries_package_commit_and_schema() {
        assert_eq!(
            describe("0.1.0", Some("abc1234"), 18),
            "0.1.0 (commit abc1234, schema 18)"
        );
    }

    #[test]
    fn no_commit_is_unknown() {
        assert_eq!(
            describe("0.1.0", None, 18),
            "0.1.0 (commit unknown, schema 18)"
        );
    }

    #[test]
    fn a_blank_commit_is_unknown_too() {
        for blank in ["", "  ", "\n"] {
            assert_eq!(
                describe("0.1.0", Some(blank), 18),
                "0.1.0 (commit unknown, schema 18)"
            );
        }
    }

    #[test]
    fn a_commit_with_a_stray_newline_is_trimmed() {
        assert_eq!(
            describe("0.1.0", Some("abc1234\n"), 18),
            "0.1.0 (commit abc1234, schema 18)"
        );
    }

    #[test]
    fn this_build_reports_the_schema_it_migrates_to() {
        assert!(version().contains(&format!("schema {}", banto_io::store::SCHEMA_VERSION)));
        assert!(version().starts_with(env!("CARGO_PKG_VERSION")));
    }
}
