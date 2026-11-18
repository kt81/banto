//! Advisory-only reading of whether a product has already been told to
//! trust the directory a fresh brigade member is about to launch into.
//!
//! Separate from [`crate::codex_trust`], which answers a completely
//! different question (has *banto's own SessionStart hook* been trusted —
//! a one-time, machine-wide record with no cwd in it at all). This module
//! is about each product's own, per-directory "trust this workspace?"
//! prompt — Codex's and Claude Code's each block on stdin/stdout the same
//! way an unanswered approval would, and neither is dismissed by
//! `--dangerously-bypass-hook-trust` or any flag banto already passes.
//!
//! Read-only, same as every other access under a product home or the user's
//! own home directory: banto never writes to `~/.codex`, `~/.claude`, or
//! `~/.claude.json` (invariant 1).
//!
//! # Why this can only ever be a hint, never a guarantee
//!
//! Both products' own records are internal state banto reads by watching a
//! file over their shoulder, not through any API either product publishes:
//! a future release can change the shape without notice, and this module
//! would then degrade to [`DirectoryTrust::Unknown`] rather than silently
//! misreport — see [`codex_directory_trust`]/[`claude_directory_trust`] for
//! where that degradation happens. Measured, not assumed, on this machine:
//!
//! - **Codex** keys `[projects.'<path>']` in `<codex_home>/config.toml` by a
//!   **lowercased**, backslash-separated, no-trailing-separator path (e.g.
//!   `[projects.'c:\users\kt81\projects\banto']`, `trust_level = "trusted"`
//!   when trusted) — every real entry observed here was already lowercase,
//!   so lowercasing before comparing is required, not defensive. The key is
//!   the *resolved* directory: Codex 0.169.0 started through a symlink
//!   (`LinkGit2`, a git repository) wrote the target's lowercased path, and
//!   started through a symlink to an already-trusted directory it did not ask
//!   again. So the spelling banto spawned with is not what the key holds.
//! - **Claude Code** keys `.projects` in `~/.claude.json` by whatever exact
//!   string it happened to receive as a cwd at some past launch, with
//!   **no normalization at all**: the same real directory was found stored
//!   under both `C:\Users\kt81\...` (backslashes, mixed case preserved) and
//!   `C:/Users/kt81/...` (forward slashes) as two distinct keys — and, for
//!   two of those directories, the two forms *disagreed* on
//!   `hasTrustDialogAccepted`. [`claude_directory_trust`] treats any
//!   matching key reporting `true` as trusted, which is deliberately
//!   optimistic: this signal only ever drives a notice to the operator (see
//!   `crate::embedded::emporium`'s untrusted-Worker status line), never a
//!   blind keystroke the way the Codex-side check does, so a false
//!   "trusted" here costs a missed notice, not a security-relevant one.
//!
//! A key that is not the same text as the directory can still name it, when
//! the directory was reached through a symlink or typed in another case: that
//! is tried second, against the spellings [`DirectorySpellings`] lists for the
//! directory, and only when the lexical comparison has not decided. No key is
//! ever opened: a registry carries stale `\\server\share\…` projects, and
//! asking the filesystem about one froze this check for 21 s (measured), on
//! every discovery pass. Only `cwd` is touched: once per call that gets that
//! far, or not at all through the `_in` entry points, which take the spellings
//! of a directory a caller already measured.

use std::borrow::Cow;
use std::path::Path;

use crate::claude_home::ClaudeHome;
use crate::codex_home::CodexHome;
use crate::file_identity::DirectorySpellings;

/// See the module doc for why this is advisory, not authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryTrust {
    /// A record for this directory (by normalized spelling, else by one of
    /// the spellings its directory is known by) says trusted.
    Trusted,
    /// A record for this directory exists and says otherwise.
    NotTrusted,
    /// No record for this directory at all, or the registry couldn't be
    /// read/parsed — this product may never have seen it before, so a
    /// launch there could still hit a first-run trust prompt.
    Unknown,
}

/// Normalizes a Windows path for cross-representation comparison: strips a
/// `\\?\` extended-length prefix, unifies `/` and `\`, lowercases (Windows
/// paths are case-insensitive), and drops a trailing separator. Both real
/// registries this module reads were observed disagreeing with each other
/// (and, for Claude Code, with themselves) on exactly these three axes.
fn normalize_path(raw: &str) -> String {
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(raw);
    let mut normalized = stripped.replace('/', "\\").to_ascii_lowercase();
    while normalized.len() > 1 && normalized.ends_with('\\') {
        normalized.pop();
    }
    normalized
}

/// Whether Codex has already been told to trust `cwd`, read from
/// `<codex_home>/config.toml`'s `[projects.'<path>'] trust_level`. `Unknown`
/// on a missing/unopenable/unparsable file, or a `trust_level` value that
/// isn't a string this function recognizes — see the module doc.
pub fn codex_directory_trust(codex_home: &CodexHome, cwd: &Path) -> DirectoryTrust {
    let Ok(text) = std::fs::read_to_string(codex_home.config_path()) else {
        return DirectoryTrust::Unknown;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return DirectoryTrust::Unknown;
    };
    codex_directory_trust_from_value(&value, cwd, || Cow::Owned(DirectorySpellings::of(cwd)))
}

/// [`codex_directory_trust`] for a caller that already holds `cwd`'s
/// spellings (the discovery tracker measured them at the spawn), so no pass
/// measures them again.
pub fn codex_directory_trust_in(
    codex_home: &CodexHome,
    cwd: &Path,
    spawned: &DirectorySpellings,
) -> DirectoryTrust {
    let Ok(text) = std::fs::read_to_string(codex_home.config_path()) else {
        return DirectoryTrust::Unknown;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return DirectoryTrust::Unknown;
    };
    codex_directory_trust_from_value(&value, cwd, || Cow::Borrowed(spawned))
}

/// `spawned` is asked for only when the lexical comparison has not decided, so
/// the plain entry point pays for the measurement on a miss and no more.
fn codex_directory_trust_from_value<'a>(
    value: &toml::Value,
    cwd: &Path,
    spawned: impl FnOnce() -> Cow<'a, DirectorySpellings>,
) -> DirectoryTrust {
    let target = normalize_path(&cwd.to_string_lossy());
    let Some(projects) = value
        .as_table()
        .and_then(|top| top.get("projects"))
        .and_then(toml::Value::as_table)
    else {
        return DirectoryTrust::Unknown;
    };
    let trust_of = |entry: &toml::Value| match entry
        .as_table()
        .and_then(|t| t.get("trust_level"))
        .and_then(toml::Value::as_str)
    {
        Some("trusted") => DirectoryTrust::Trusted,
        Some(_) => DirectoryTrust::NotTrusted,
        None => DirectoryTrust::Unknown,
    };
    if let Some((_, entry)) = projects
        .iter()
        .find(|(key, _)| normalize_path(key) == target)
    {
        return trust_of(entry);
    }
    let spawned = spawned();
    projects
        .iter()
        .find(|(key, _)| spawned.names(Path::new(key.as_str())))
        .map_or(DirectoryTrust::Unknown, |(_, entry)| trust_of(entry))
}

/// Whether Claude Code has already been told to trust `cwd`, read from
/// `~/.claude.json`'s `.projects[<path>].hasTrustDialogAccepted`. `Unknown`
/// on a missing/unopenable/unparsable file or a document with no `projects`
/// object at all — see the module doc for the optimistic any-match-wins
/// policy this uses once a key does match.
pub fn claude_directory_trust(claude_home: &ClaudeHome, cwd: &Path) -> DirectoryTrust {
    let Ok(text) = std::fs::read_to_string(claude_home.trust_registry_path()) else {
        return DirectoryTrust::Unknown;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return DirectoryTrust::Unknown;
    };
    claude_directory_trust_from_value(&value, cwd, || Cow::Owned(DirectorySpellings::of(cwd)))
}

/// [`claude_directory_trust`] for a caller that already holds `cwd`'s
/// spellings; see [`codex_directory_trust_in`].
pub fn claude_directory_trust_in(
    claude_home: &ClaudeHome,
    cwd: &Path,
    spawned: &DirectorySpellings,
) -> DirectoryTrust {
    let Ok(text) = std::fs::read_to_string(claude_home.trust_registry_path()) else {
        return DirectoryTrust::Unknown;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return DirectoryTrust::Unknown;
    };
    claude_directory_trust_from_value(&value, cwd, || Cow::Borrowed(spawned))
}

fn claude_directory_trust_from_value<'a>(
    value: &serde_json::Value,
    cwd: &Path,
    spawned: impl FnOnce() -> Cow<'a, DirectorySpellings>,
) -> DirectoryTrust {
    let target = normalize_path(&cwd.to_string_lossy());
    let Some(projects) = value.get("projects").and_then(serde_json::Value::as_object) else {
        return DirectoryTrust::Unknown;
    };
    let accepted = |entry: &serde_json::Value| {
        entry.get("hasTrustDialogAccepted") == Some(&serde_json::Value::Bool(true))
    };
    let mut matched = false;
    for (key, entry) in projects {
        if normalize_path(key) != target {
            continue;
        }
        matched = true;
        if accepted(entry) {
            return DirectoryTrust::Trusted;
        }
    }
    // Any matching key reporting true wins, so a lexical match that says false
    // does not end the search: another spelling of the directory may say true.
    let spawned = spawned();
    for (key, entry) in projects {
        if normalize_path(key) == target || !spawned.names(Path::new(key)) {
            continue;
        }
        matched = true;
        if accepted(entry) {
            return DirectoryTrust::Trusted;
        }
    }
    if matched {
        DirectoryTrust::NotTrusted
    } else {
        DirectoryTrust::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // -- normalize_path ----------------------------------------------------

    #[test]
    fn normalize_path_unifies_case_separators_and_a_trailing_slash() {
        assert_eq!(
            normalize_path(r"C:\Users\kt81\Projects\banto"),
            "c:\\users\\kt81\\projects\\banto"
        );
        assert_eq!(
            normalize_path("C:/Users/kt81/Projects/banto/"),
            "c:\\users\\kt81\\projects\\banto"
        );
        assert_eq!(
            normalize_path(r"\\?\C:\Users\kt81\Projects\banto"),
            "c:\\users\\kt81\\projects\\banto"
        );
    }

    // -- codex_directory_trust ----------------------------------------------

    fn codex_home_with_config(dir: &TempDir, config_text: &str) -> CodexHome {
        let home = CodexHome::new(dir.path().to_path_buf());
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(home.config_path(), config_text).unwrap();
        home
    }

    #[test]
    fn codex_directory_trust_is_trusted_for_a_matching_entry() {
        let dir = TempDir::new().unwrap();
        let home = codex_home_with_config(
            &dir,
            "[projects.'c:\\users\\kt81\\projects\\banto']\ntrust_level = \"trusted\"\n",
        );
        assert_eq!(
            codex_directory_trust(&home, Path::new(r"C:\Users\kt81\Projects\banto")),
            DirectoryTrust::Trusted
        );
    }

    #[test]
    fn codex_directory_trust_matches_regardless_of_case_or_separator_or_trailing_slash() {
        let dir = TempDir::new().unwrap();
        let home = codex_home_with_config(
            &dir,
            "[projects.'c:\\users\\kt81\\projects\\banto']\ntrust_level = \"trusted\"\n",
        );
        for cwd in [
            "C:/Users/kt81/Projects/banto",
            "C:/Users/kt81/Projects/banto/",
            r"c:\USERS\KT81\projects\BANTO",
            r"\\?\C:\Users\kt81\Projects\banto",
        ] {
            assert_eq!(
                codex_directory_trust(&home, Path::new(cwd)),
                DirectoryTrust::Trusted,
                "expected a match for {cwd}"
            );
        }
    }

    #[test]
    fn codex_directory_trust_is_not_trusted_for_a_recognized_but_untrusted_entry() {
        let dir = TempDir::new().unwrap();
        let home = codex_home_with_config(
            &dir,
            "[projects.'c:\\work\\alpha']\ntrust_level = \"untrusted\"\n",
        );
        assert_eq!(
            codex_directory_trust(&home, Path::new(r"C:\work\alpha")),
            DirectoryTrust::NotTrusted
        );
    }

    #[test]
    fn codex_directory_trust_is_unknown_for_a_directory_with_no_entry() {
        let dir = TempDir::new().unwrap();
        let home = codex_home_with_config(
            &dir,
            "[projects.'c:\\work\\alpha']\ntrust_level = \"trusted\"\n",
        );
        assert_eq!(
            codex_directory_trust(&home, Path::new(r"C:\work\beta")),
            DirectoryTrust::Unknown
        );
    }

    #[test]
    fn codex_directory_trust_is_unknown_when_config_is_missing() {
        let dir = TempDir::new().unwrap();
        let home = CodexHome::new(dir.path().to_path_buf());
        assert_eq!(
            codex_directory_trust(&home, Path::new(r"C:\work\alpha")),
            DirectoryTrust::Unknown
        );
    }

    #[test]
    fn codex_directory_trust_is_unknown_when_config_is_malformed() {
        let dir = TempDir::new().unwrap();
        let home = codex_home_with_config(&dir, "this is not [valid toml");
        assert_eq!(
            codex_directory_trust(&home, Path::new(r"C:\work\alpha")),
            DirectoryTrust::Unknown
        );
    }

    #[test]
    fn codex_directory_trust_is_unknown_when_projects_table_is_absent() {
        let dir = TempDir::new().unwrap();
        let home = codex_home_with_config(&dir, "some_other_setting = 1\n");
        assert_eq!(
            codex_directory_trust(&home, Path::new(r"C:\work\alpha")),
            DirectoryTrust::Unknown
        );
    }

    // -- claude_directory_trust ----------------------------------------------

    fn claude_home_with_registry(dir: &TempDir, registry_text: &str) -> ClaudeHome {
        let root = dir.path().join(".claude");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join(".claude.json"), registry_text).unwrap();
        ClaudeHome::new(root)
    }

    #[test]
    fn claude_directory_trust_is_trusted_for_a_matching_entry() {
        let dir = TempDir::new().unwrap();
        let home = claude_home_with_registry(
            &dir,
            r#"{"projects": {"C:\\Users\\kt81\\Projects\\banto": {"hasTrustDialogAccepted": true}}}"#,
        );
        assert_eq!(
            claude_directory_trust(&home, Path::new(r"C:\Users\kt81\Projects\banto")),
            DirectoryTrust::Trusted
        );
    }

    #[test]
    fn claude_directory_trust_matches_regardless_of_case_or_separator() {
        let dir = TempDir::new().unwrap();
        let home = claude_home_with_registry(
            &dir,
            r#"{"projects": {"C:/Users/kt81/Projects/banto": {"hasTrustDialogAccepted": true}}}"#,
        );
        assert_eq!(
            claude_directory_trust(&home, Path::new(r"c:\users\kt81\projects\banto")),
            DirectoryTrust::Trusted
        );
    }

    #[test]
    fn claude_directory_trust_any_matching_entry_reporting_true_wins() {
        // Real, measured disagreement: the same directory recorded under
        // both slash conventions, one true and one false.
        let dir = TempDir::new().unwrap();
        let home = claude_home_with_registry(
            &dir,
            r#"{"projects": {
                "C:\\Users\\kt81": {"hasTrustDialogAccepted": false},
                "C:/Users/kt81": {"hasTrustDialogAccepted": true}
            }}"#,
        );
        assert_eq!(
            claude_directory_trust(&home, Path::new(r"C:\Users\kt81")),
            DirectoryTrust::Trusted
        );
    }

    #[test]
    fn claude_directory_trust_is_not_trusted_when_every_matching_entry_says_false() {
        let dir = TempDir::new().unwrap();
        let home = claude_home_with_registry(
            &dir,
            r#"{"projects": {"C:\\Users\\kt81": {"hasTrustDialogAccepted": false}}}"#,
        );
        assert_eq!(
            claude_directory_trust(&home, Path::new(r"C:\Users\kt81")),
            DirectoryTrust::NotTrusted
        );
    }

    #[test]
    fn claude_directory_trust_is_unknown_for_a_directory_with_no_entry() {
        let dir = TempDir::new().unwrap();
        let home = claude_home_with_registry(
            &dir,
            r#"{"projects": {"C:\\work\\alpha": {"hasTrustDialogAccepted": true}}}"#,
        );
        assert_eq!(
            claude_directory_trust(&home, Path::new(r"C:\work\beta")),
            DirectoryTrust::Unknown
        );
    }

    #[test]
    fn claude_directory_trust_is_unknown_when_the_registry_is_missing() {
        let dir = TempDir::new().unwrap();
        let home = ClaudeHome::new(dir.path().join(".claude"));
        assert_eq!(
            claude_directory_trust(&home, Path::new(r"C:\work\alpha")),
            DirectoryTrust::Unknown
        );
    }

    #[test]
    fn claude_directory_trust_is_unknown_when_the_registry_is_malformed() {
        let dir = TempDir::new().unwrap();
        let home = claude_home_with_registry(&dir, "{not valid json");
        assert_eq!(
            claude_directory_trust(&home, Path::new(r"C:\work\alpha")),
            DirectoryTrust::Unknown
        );
    }

    #[test]
    fn claude_directory_trust_is_unknown_when_projects_object_is_absent() {
        let dir = TempDir::new().unwrap();
        let home = claude_home_with_registry(&dir, r#"{"someOtherKey": 1}"#);
        assert_eq!(
            claude_directory_trust(&home, Path::new(r"C:\work\alpha")),
            DirectoryTrust::Unknown
        );
    }

    // -- a key spelled another way than the directory ------------------------

    /// A directory and a symlink to it, or `None` (the helper says why).
    fn directory_and_symlink(work: &TempDir) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let real = work.path().canonicalize().unwrap().join("real");
        let link = work.path().join("link");
        std::fs::create_dir(&real).unwrap();
        crate::file_identity::test_support::symlink_dir(&real, &link).then_some((real, link))
    }

    /// `path` without a `\\?\` prefix, which neither registry holds.
    fn plain(path: &Path) -> String {
        let text = path.to_string_lossy();
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
    }

    /// Codex's key as measured: the resolved path, lowercased. Lowercased on
    /// Windows only, where a volume folds case; elsewhere that is another path.
    fn codex_key(resolved: &Path) -> String {
        if cfg!(windows) {
            plain(resolved).to_lowercase()
        } else {
            plain(resolved)
        }
    }

    /// Claude's key as measured: whatever string it was given, which on Windows
    /// is sometimes the path with forward slashes.
    fn claude_key(resolved: &Path) -> String {
        if cfg!(windows) {
            plain(resolved).replace('\\', "/")
        } else {
            plain(resolved)
        }
    }

    #[test]
    fn plain_drops_the_verbatim_prefix() {
        assert_eq!(plain(Path::new(r"\\?\C:\work")), r"C:\work");
        assert_eq!(plain(Path::new(r"C:\work")), r"C:\work");
    }

    /// Codex 0.169.0 keys a directory it was started through a symlink by the
    /// resolved path, so the spelling banto spawned with is not in the table.
    #[test]
    fn codex_directory_trust_finds_the_key_of_the_directory_a_symlink_leads_to() {
        let dir = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let Some((real, link)) = directory_and_symlink(&work) else {
            return;
        };
        let home = codex_home_with_config(
            &dir,
            &format!(
                "[projects.'{}']\ntrust_level = \"trusted\"\n",
                codex_key(&real)
            ),
        );
        assert_eq!(codex_directory_trust(&home, &link), DirectoryTrust::Trusted);
        assert_eq!(
            codex_directory_trust(&home, &work.path().join("elsewhere")),
            DirectoryTrust::Unknown
        );
    }

    #[test]
    fn claude_directory_trust_finds_the_key_of_the_directory_a_symlink_leads_to() {
        let dir = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let Some((real, link)) = directory_and_symlink(&work) else {
            return;
        };
        let registry = serde_json::json!({"projects": {
            claude_key(&real): {"hasTrustDialogAccepted": true}
        }});
        let home = claude_home_with_registry(&dir, &registry.to_string());
        assert_eq!(
            claude_directory_trust(&home, &link),
            DirectoryTrust::Trusted
        );
    }

    /// A key that matches by spelling and says false must not hide another
    /// spelling of the same directory that says true.
    #[test]
    fn claude_directory_trust_lets_a_true_key_of_another_spelling_win() {
        let dir = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let Some((real, link)) = directory_and_symlink(&work) else {
            return;
        };
        let registry = serde_json::json!({"projects": {
            plain(&link): {"hasTrustDialogAccepted": false},
            claude_key(&real): {"hasTrustDialogAccepted": true}
        }});
        let home = claude_home_with_registry(&dir, &registry.to_string());
        assert_eq!(
            claude_directory_trust(&home, &link),
            DirectoryTrust::Trusted
        );
    }

    #[test]
    fn claude_directory_trust_is_not_trusted_when_only_another_spelling_says_false() {
        let dir = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let Some((real, link)) = directory_and_symlink(&work) else {
            return;
        };
        let registry = serde_json::json!({"projects": {
            claude_key(&real): {"hasTrustDialogAccepted": false}
        }});
        let home = claude_home_with_registry(&dir, &registry.to_string());
        assert_eq!(
            claude_directory_trust(&home, &link),
            DirectoryTrust::NotTrusted
        );
    }

    /// A project on a share that is gone must be answered as text. Opening it
    /// took 21 s on this machine (`\\192.0.2.1`, an address nothing answers),
    /// on every discovery pass. The bound is far from that, so no timing race.
    #[test]
    fn a_registry_key_on_an_unreachable_share_is_not_opened() {
        let dir = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let registry = serde_json::json!({"projects": {
            r"\\192.0.2.1\share\proj": {"hasTrustDialogAccepted": true},
            r"\\?\UNC\192.0.2.1\share\proj2": {"hasTrustDialogAccepted": true}
        }});
        let claude = claude_home_with_registry(&dir, &registry.to_string());
        let codex = codex_home_with_config(
            &dir,
            "[projects.'\\\\192.0.2.1\\share\\proj']\ntrust_level = \"trusted\"\n",
        );
        let started = std::time::Instant::now();
        assert_eq!(
            claude_directory_trust(&claude, work.path()),
            DirectoryTrust::Unknown
        );
        assert_eq!(
            codex_directory_trust(&codex, work.path()),
            DirectoryTrust::Unknown
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// The `_in` entry points compare against the spellings they are handed, not
    /// against ones they measure: a pure `from_parts` list names a directory
    /// that does not exist here, and `cwd` itself is not a key.
    #[test]
    fn the_in_entry_points_use_the_spellings_they_are_given() {
        let dir = TempDir::new().unwrap();
        let cwd = Path::new("/work/typed");
        let spawned = DirectorySpellings::from_parts(&[cwd, Path::new("/work/resolved")], false);
        let claude = claude_home_with_registry(
            &dir,
            r#"{"projects": {"/work/resolved": {"hasTrustDialogAccepted": true}}}"#,
        );
        let codex = codex_home_with_config(
            &dir,
            "[projects.'/work/resolved']\ntrust_level = \"trusted\"\n",
        );
        assert_eq!(
            claude_directory_trust_in(&claude, cwd, &spawned),
            DirectoryTrust::Trusted
        );
        assert_eq!(
            codex_directory_trust_in(&codex, cwd, &spawned),
            DirectoryTrust::Trusted
        );
        assert_eq!(
            claude_directory_trust(&claude, cwd),
            DirectoryTrust::Unknown
        );
    }
}
