//! Advisory-only reading of whether Claude Code has already been told to trust
//! the directory a fresh Claude Worker is about to launch into.
//!
//! Separate from [`crate::codex_trust`], which answers a completely
//! different question (has *banto's own SessionStart hook* been trusted —
//! a one-time, machine-wide record with no cwd in it at all). This module
//! is about Claude Code's own, per-directory "trust this workspace?" prompt,
//! which blocks on stdin the same way an unanswered approval would and is not
//! dismissed by any flag banto already passes.
//!
//! Read-only, same as every other access under a product home or the user's
//! own home directory: banto never writes to `~/.claude` or `~/.claude.json`
//! (invariant 1).
//!
//! # Only ever a hint, and only ever for a notice
//!
//! Its one reader drives the status line that says a Claude Worker looks stuck
//! behind that prompt. It once also gated the first line banto types into a
//! Codex Worker and a Goinkyo, and that failed both ways: Claude never records
//! trust for the home directory, and Codex was seen to write no trust key outside a
//! git repository, so an already-trusted pane waited for ever, while a stale
//! `true` typed into a screen that was not a prompt. That first line is now a
//! command-line argument, which waits for nothing, so this registry gates no
//! keystroke. A registry like this is internal state banto
//! reads over the product's shoulder, not an API: it can say nothing about the
//! pane in front of the operator, so nothing may type on its word.
//!
//! The registry is internal state banto reads by watching a file, so a future
//! release can change the shape without notice, and this module would then
//! degrade to [`DirectoryTrust::Unknown`] rather than silently misreport.
//! Measured, not assumed, on this machine: Claude Code keys `.projects` in
//! `~/.claude.json` by whatever exact string it happened to receive as a cwd at
//! some past launch, with **no normalization at all**: the same real directory
//! was found stored under both `C:\Users\kt81\...` (backslashes, mixed case
//! preserved) and `C:/Users/kt81/...` (forward slashes) as two distinct keys —
//! and, for two of those directories, the two forms *disagreed* on
//! `hasTrustDialogAccepted`. [`claude_directory_trust_in`] treats any matching
//! key reporting `true` as trusted, which is deliberately optimistic: a false
//! "trusted" here costs a missed notice, nothing more.
//!
//! A key that is not the same text as the directory can still name it, when
//! the directory was reached through a symlink or typed in another case: that
//! is tried second, against the spellings [`DirectorySpellings`] lists for the
//! directory, and only when the lexical comparison has not decided. No key is
//! ever opened: a registry carries stale `\\server\share\…` projects, and
//! asking the filesystem about one froze this check for 21 s (measured), on
//! every discovery pass. Only `cwd` is touched, and the caller hands in the
//! spellings of a directory it already measured.

use std::borrow::Cow;
use std::path::Path;

use crate::claude_home::ClaudeHome;
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
/// paths are case-insensitive), and drops a trailing separator. Claude Code's
/// registry was observed disagreeing with itself on exactly these three axes.
fn normalize_path(raw: &str) -> String {
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(raw);
    let mut normalized = stripped.replace('/', "\\").to_ascii_lowercase();
    while normalized.len() > 1 && normalized.ends_with('\\') {
        normalized.pop();
    }
    normalized
}

/// Whether Claude Code has already been told to trust `cwd`, read from
/// `.projects[<path>].hasTrustDialogAccepted` in the `.claude.json` the agent
/// uses (`~/.claude.json`, or inside `$CLAUDE_CONFIG_DIR`; see
/// [`ClaudeHome::trust_registry_path`]). `spawned` is
/// `cwd`'s spellings, which the discovery tracker measured at the spawn so no
/// pass measures them again. `Unknown` on a missing/unopenable/unparsable file
/// or a document with no `projects` object at all — see the module doc for the
/// optimistic any-match-wins policy this uses once a key does match.
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

/// `spawned` is asked for only when the lexical comparison has not decided.
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

    // -- claude_directory_trust_in -------------------------------------------

    /// What a caller with no measurement of its own would ask: the spellings of
    /// `cwd` itself, measured here.
    fn trust(home: &ClaudeHome, cwd: &Path) -> DirectoryTrust {
        claude_directory_trust_in(home, cwd, &DirectorySpellings::of(cwd))
    }

    fn claude_home_with_registry(dir: &TempDir, registry_text: &str) -> ClaudeHome {
        let root = dir.path().join(".claude");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join(".claude.json"), registry_text).unwrap();
        ClaudeHome::new(root)
    }

    /// A registry with one project, the directory the next two tests ask about.
    const SYNTHETIC_CWD: &str = r"C:\Users\synthetic\Projects\banto";
    const SYNTHETIC_TRUSTED: &str = r#"{"projects": {"C:\\Users\\synthetic\\Projects\\banto": {"hasTrustDialogAccepted": true}}}"#;
    const SYNTHETIC_REFUSED: &str = r#"{"projects": {"C:\\Users\\synthetic\\Projects\\banto": {"hasTrustDialogAccepted": false}}}"#;

    /// The home an agent with `CLAUDE_CONFIG_DIR=<work>` has.
    fn home_with_config_dir(work: &Path) -> ClaudeHome {
        let value = work.to_string_lossy().into_owned();
        ClaudeHome::default_home_from_env(move |key| {
            (key == "CLAUDE_CONFIG_DIR").then(|| value.clone())
        })
        .unwrap()
    }

    #[test]
    fn a_claude_config_dir_is_read_from_inside_that_directory_not_beside_it() {
        // The multi-account layout: the work account's file is in its own
        // directory, and `<parent>/.claude.json` is another account's, which here
        // trusts the directory the work account has refused.
        let dir = TempDir::new().unwrap();
        let work = dir.path().join(".claude-work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join(".claude.json"), SYNTHETIC_REFUSED).unwrap();
        std::fs::write(dir.path().join(".claude.json"), SYNTHETIC_TRUSTED).unwrap();
        assert_eq!(
            trust(&home_with_config_dir(&work), Path::new(SYNTHETIC_CWD)),
            DirectoryTrust::NotTrusted
        );
    }

    #[test]
    fn a_missing_registry_under_a_claude_config_dir_is_unknown_not_the_neighbours_answer() {
        let dir = TempDir::new().unwrap();
        let work = dir.path().join(".claude-work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(dir.path().join(".claude.json"), SYNTHETIC_TRUSTED).unwrap();
        assert_eq!(
            trust(&home_with_config_dir(&work), Path::new(SYNTHETIC_CWD)),
            DirectoryTrust::Unknown
        );
    }

    #[test]
    fn claude_directory_trust_is_trusted_for_a_matching_entry() {
        let dir = TempDir::new().unwrap();
        let home = claude_home_with_registry(
            &dir,
            r#"{"projects": {"C:\\Users\\kt81\\Projects\\banto": {"hasTrustDialogAccepted": true}}}"#,
        );
        assert_eq!(
            trust(&home, Path::new(r"C:\Users\kt81\Projects\banto")),
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
            trust(&home, Path::new(r"c:\users\kt81\projects\banto")),
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
            trust(&home, Path::new(r"C:\Users\kt81")),
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
            trust(&home, Path::new(r"C:\Users\kt81")),
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
            trust(&home, Path::new(r"C:\work\beta")),
            DirectoryTrust::Unknown
        );
    }

    #[test]
    fn claude_directory_trust_is_unknown_when_the_registry_is_missing() {
        let dir = TempDir::new().unwrap();
        let home = ClaudeHome::new(dir.path().join(".claude"));
        assert_eq!(
            trust(&home, Path::new(r"C:\work\alpha")),
            DirectoryTrust::Unknown
        );
    }

    #[test]
    fn claude_directory_trust_is_unknown_when_the_registry_is_malformed() {
        let dir = TempDir::new().unwrap();
        let home = claude_home_with_registry(&dir, "{not valid json");
        assert_eq!(
            trust(&home, Path::new(r"C:\work\alpha")),
            DirectoryTrust::Unknown
        );
    }

    #[test]
    fn claude_directory_trust_is_unknown_when_projects_object_is_absent() {
        let dir = TempDir::new().unwrap();
        let home = claude_home_with_registry(&dir, r#"{"someOtherKey": 1}"#);
        assert_eq!(
            trust(&home, Path::new(r"C:\work\alpha")),
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
        assert_eq!(trust(&home, &link), DirectoryTrust::Trusted);
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
        assert_eq!(trust(&home, &link), DirectoryTrust::Trusted);
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
        assert_eq!(trust(&home, &link), DirectoryTrust::NotTrusted);
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
        let started = std::time::Instant::now();
        assert_eq!(trust(&claude, work.path()), DirectoryTrust::Unknown);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// `claude_directory_trust_in` compares against the spellings it is handed,
    /// not against ones it measures: a pure `from_parts` list names a directory
    /// that does not exist here, and `cwd` itself is not a key.
    #[test]
    fn the_in_entry_point_uses_the_spellings_it_is_given() {
        let dir = TempDir::new().unwrap();
        let cwd = Path::new("/work/typed");
        let spawned = DirectorySpellings::from_parts(&[cwd, Path::new("/work/resolved")], false);
        let claude = claude_home_with_registry(
            &dir,
            r#"{"projects": {"/work/resolved": {"hasTrustDialogAccepted": true}}}"#,
        );
        assert_eq!(
            claude_directory_trust_in(&claude, cwd, &spawned),
            DirectoryTrust::Trusted
        );
        assert_eq!(trust(&claude, cwd), DirectoryTrust::Unknown);
    }
}
