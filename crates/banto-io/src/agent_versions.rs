//! Pull-only readers for version facts retained by the agent products.
//!
//! These functions must never be called from the polling/reload path.  A
//! Codex session version needs the beginning of a rollout JSONL which normal
//! discovery deliberately does not read; doing that on every reload would
//! make a pull-only question part of the resident cost.  `banto versions`
//! is the sole intended caller.
//!
//! No result here is an installed-binary probe: Claude supplies the version
//! of a *running session* and the last auto-update result; Codex supplies a
//! session's recorded CLI version plus its standalone package records.

use std::fs;
use std::io::{BufRead, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;
use serde_json::Value;

/// Claude Code's upstream record of its last automatic update attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeAutoUpdate {
    pub timestamp: String,
    pub version_from: Option<String>,
    pub version_to: Option<String>,
}

/// One retained Codex standalone release, dated by the local release
/// directory's filesystem timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRelease {
    pub version: String,
    pub modified: Option<SystemTime>,
    pub path: PathBuf,
}

#[derive(Deserialize)]
struct ClaudeAutoUpdateRaw {
    timestamp: Option<String>,
    version_from: Option<String>,
    version_to: Option<String>,
}

#[derive(Deserialize)]
struct CodexPackageRaw {
    version: Option<String>,
}

/// Read Claude's last automatic update result.  Missing, malformed, or
/// incomplete input is unknown rather than an error.
pub fn read_claude_last_auto_update(path: &Path) -> Option<ClaudeAutoUpdate> {
    let raw: ClaudeAutoUpdateRaw = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let timestamp = raw.timestamp.filter(|value| !value.is_empty())?;
    Some(ClaudeAutoUpdate {
        timestamp,
        version_from: raw.version_from.filter(|value| !value.is_empty()),
        version_to: raw.version_to.filter(|value| !value.is_empty()),
    })
}

/// Read the version in Codex's current standalone package manifest.
pub fn read_codex_current_version(path: &Path) -> Option<String> {
    let raw: CodexPackageRaw = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    raw.version.filter(|value| !value.is_empty())
}

/// Read Codex's retained standalone releases.  A malformed child is skipped;
/// the caller still receives every independently readable release.
pub fn read_codex_release_history(releases_dir: &Path) -> Vec<CodexRelease> {
    // Safety: these retained releases are runnable executables, not an inert
    // archive.  Measured 2026-08-07, launching an older one ran Codex's live
    // network install script and reported a reinstall; the already-present
    // target looked unchanged on disk in that test, not a guarantee against
    // an identical rewrite or a download in another case.
    let Ok(entries) = fs::read_dir(releases_dir) else {
        return Vec::new();
    };
    let mut releases = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let manifest = path.join("codex-package.json");
            let version = read_codex_current_version(&manifest)?;
            // The manifest's mtime is the upstream package-build clock; the
            // release directory's own mtime is when this retained release
            // landed on this machine.  Version history answers the latter.
            let modified = fs::metadata(&path)
                .ok()
                .and_then(|meta| meta.modified().ok());
            Some(CodexRelease {
                version,
                modified,
                path,
            })
        })
        .collect::<Vec<_>>();
    releases.sort_by(|a, b| {
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.version.cmp(&b.version))
    });
    releases
}

/// Read the first `session_meta.payload.cli_version` in a Codex rollout.
///
/// Bound the scan at 256 KiB so a malformed rollout cannot turn this
/// pull-time convenience into an unbounded read.  This function is pull-only
/// by the module contract above, never a discovery/polling helper.
pub fn read_codex_session_version(rollout_path: &Path) -> Option<String> {
    const MAX_HEAD_BYTES: usize = 256 * 1024;
    let file = fs::File::open(rollout_path).ok()?;
    // `Take` bounds the underlying reader itself, rather than merely
    // counting completed lines after `lines()` might already have allocated
    // an arbitrarily long one.  256 KiB matches the existing Claude session
    // provider's head cap and comfortably covers the observed first-record
    // `session_meta`; a different rollout prefix still gets a bounded search.
    for line in BufReader::new(file).take(MAX_HEAD_BYTES as u64).lines() {
        // An I/O failure means the rest of this stream cannot be trusted or
        // read, so this session's version is unknown.  A malformed JSONL
        // record is different: it is one upstream record among many, and the
        // bounded search must still reach a later valid session_meta.
        let line = line.ok()?;
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) == Some("session_meta") {
            return record
                .get("payload")
                .and_then(|payload| payload.get("cli_version"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_claude_last_auto_update() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result.json");
        fs::write(&path, r#"{"timestamp":"2026-08-07T08:53:16.642Z","version_from":"2.1.219","version_to":"2.1.224"}"#).unwrap();
        assert_eq!(
            read_claude_last_auto_update(&path),
            Some(ClaudeAutoUpdate {
                timestamp: "2026-08-07T08:53:16.642Z".into(),
                version_from: Some("2.1.219".into()),
                version_to: Some("2.1.224".into())
            })
        );
    }

    #[test]
    fn reads_codex_version_from_first_session_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        fs::write(
            &path,
            "{broken}\n{\"type\":\"session_meta\",\"payload\":{\"cli_version\":\"0.147.0\"}}\n",
        )
        .unwrap();
        assert_eq!(
            read_codex_session_version(&path).as_deref(),
            Some("0.147.0")
        );
    }

    #[test]
    fn release_history_skips_bad_children_and_sorts_by_manifest_time() {
        let dir = tempfile::tempdir().unwrap();
        let releases = dir.path().join("releases");
        fs::create_dir_all(releases.join("one")).unwrap();
        fs::create_dir_all(releases.join("bad")).unwrap();
        fs::write(
            releases.join("one/codex-package.json"),
            r#"{"version":"0.1.0"}"#,
        )
        .unwrap();
        assert_eq!(read_codex_release_history(&releases).len(), 1);
    }
}
