//! Session lineage resolution — beside the provider: turns a discovered
//! session's `continuation_of_uuid` (an auto-compaction fork's
//! `logicalParentUuid`, captured by `provider::claude_code`) into a
//! resolved parent id, cached forever via `store::lineage`.
//!
//! Resolution is expensive — a streaming scan of every other `.jsonl` file
//! in the child's project directory, tens of MB each — and lineage is
//! immutable once found, so callers must budget it: [`resolve_lineage`]
//! attempts at most [`RESOLUTION_BUDGET`] unresolved children per call. A
//! child whose scan finds nothing goes into the caller-owned `failed` set
//! so it is not retried again this run; only a fresh process, starting
//! from a fresh empty set, retries it.
//!
//! A `"uuid":"U"` byte match is not proof of authorship (measured on a real
//! continuation, 2026-07-26): auto-compaction copies the parent's preserved
//! tail messages into the child file WITH THEIR ORIGINAL UUIDS, listed in
//! the child's own head `compact_boundary` record's
//! `compactMetadata.preservedMessages.uuids`. So a file that merely
//! *inherited* a copy of uuid `U` is byte-indistinguishable from the file
//! that actually authored it — [`find_parent`]'s candidate scan
//! ([`classify`]) tells the two apart in one streaming pass per file and
//! prefers an original over a copy (see [`Witness`]).

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use banto_core::model::{SessionId, SessionMeta};

use crate::store::{Store, StoreError};

/// How many unresolved continuations get a resolution attempt per call.
/// Each attempt streams every other `.jsonl` file in the child's project
/// directory looking for one substring, so this stays deliberately small.
const RESOLUTION_BUDGET: usize = 2;

/// Resolves as many of `sessions`' unresolved `continuation_of_uuid`s as
/// the budget allows, recording each found link in `store`.
///
/// A session already in `failed` (a prior call this run found nothing for
/// it) is skipped without spending budget; a session already resolved
/// (`store.has_lineage`) is likewise skipped for free. A session whose scan
/// finds nothing this call is added to `failed`.
pub fn resolve_lineage(
    store: &Store,
    sessions: &[SessionMeta],
    failed: &mut HashSet<SessionId>,
) -> Result<(), StoreError> {
    let mut attempted = 0;
    for meta in sessions {
        if attempted >= RESOLUTION_BUDGET {
            break;
        }
        let Some(uuid) = meta.continuation_of_uuid.as_deref() else {
            continue;
        };
        if failed.contains(&meta.id) || store.has_lineage(&meta.id)? {
            continue;
        }
        attempted += 1;
        match find_parent(meta, uuid) {
            Some(parent_id) => store.record_lineage(&meta.id, &parent_id)?,
            None => {
                failed.insert(meta.id.clone());
            }
        }
    }
    Ok(())
}

/// Scans `.jsonl` files in `meta`'s project directory (excluding `meta`
/// itself), newest first, classifying each ([`classify`]) against `uuid`
/// and returning the first one that authored it ([`Witness::Original`]).
/// If none did, falls back to the first that merely inherited a copy of it
/// ([`Witness::Copy`]) — a preference, not an exclusion, so a chain whose
/// only witness is a copy still resolves exactly as it always has. In the
/// common case (the newest match is an original) this stops at the same
/// file the old presence-only scan would have.
fn find_parent(meta: &SessionMeta, uuid: &str) -> Option<SessionId> {
    let dir = meta.source_path.parent()?;
    let mut candidates: Vec<(SystemTime, PathBuf)> = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter(|path| path != &meta.source_path)
        .filter_map(|path| {
            let mtime = fs::metadata(&path).ok()?.modified().ok()?;
            Some((mtime, path))
        })
        .collect();
    candidates.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));

    let mut fallback_copy: Option<PathBuf> = None;
    for (_, path) in candidates {
        match classify(&path, uuid) {
            Witness::Original => return session_id_of(&path),
            Witness::Copy => {
                fallback_copy.get_or_insert(path);
            }
            Witness::Absent => {}
        }
    }
    fallback_copy.as_deref().and_then(session_id_of)
}

/// A candidate file's relationship to `uuid`, decided by [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Witness {
    /// `uuid` never appears in this file at all (reached EOF with no
    /// match).
    Absent,
    /// `uuid` appears, and no `compact_boundary` seen before that match
    /// claimed it as an inherited copy — this file is either an ordinary
    /// session or a continuation whose own preserved set doesn't include
    /// `uuid`, either way genuine authorship.
    Original,
    /// `uuid` appears, and a `compact_boundary` seen before that match
    /// lists it in `compactMetadata.preservedMessages.uuids`: copied in
    /// from wherever it was actually authored, not authored here.
    Copy,
}

/// Streams `path` once — never loading the whole file into memory, and
/// stopping the moment `uuid` is found — to classify its relationship to
/// it ([`Witness`]). Correct to decide at the first match and read no
/// further: a preserved copy can only appear *after* the `compact_boundary`
/// that lists it (the boundary is what writes the copies out beneath
/// itself), so by the time `"uuid":"{uuid}"` is seen, every boundary that
/// could possibly have claimed it has already been seen too — nothing
/// later in the file can change the verdict. Every `compact_boundary`
/// encountered so far accumulates into one preserved-uuid set (not just
/// the first: a manually-compacted session keeps its own id, so a mid-file
/// boundary is possible). This keeps the old presence-only scan's read
/// length exactly: stop at the needle, or read to EOF only when the file
/// never contains it — the common case (an ordinary file with no
/// `compact_boundary` at all) costs exactly what it always did. The
/// `contains("compact_boundary")` pre-check keeps an ordinary line from
/// paying for a JSON parse it can't possibly need.
///
/// Lenient by design (an unreadable file, a missing `compact_boundary`, an
/// unparseable one, or a missing/oddly-shaped `preservedMessages` all mean
/// "no known preserved set" — [`preserved_uuids`] already folds every one
/// of those into an empty set): a parse failure must never manufacture a
/// [`Witness::Copy`] that the old plain presence check wouldn't have
/// counted as a match at all. The only way out is [`Witness::Original`] or
/// [`Witness::Absent`].
fn classify(path: &Path, uuid: &str) -> Witness {
    let Ok(file) = File::open(path) else {
        return Witness::Absent;
    };
    let needle = format!("\"uuid\":\"{uuid}\"");
    let mut preserved: HashSet<String> = HashSet::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if line.contains(&needle) {
            return if preserved.contains(uuid) {
                Witness::Copy
            } else {
                Witness::Original
            };
        }
        if line.contains("compact_boundary")
            && let Ok(record) = serde_json::from_str::<Value>(&line)
            && record.get("type").and_then(Value::as_str) == Some("system")
            && record.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            preserved.extend(preserved_uuids(&record));
        }
    }
    Witness::Absent
}

/// A `compact_boundary` record's `compactMetadata.preservedMessages.uuids`,
/// or an empty set if any step of that path is missing or the wrong shape —
/// see [`classify`]'s leniency contract: this never returns anything that
/// would let a parse failure count as evidence of a copy.
fn preserved_uuids(record: &Value) -> HashSet<String> {
    record
        .get("compactMetadata")
        .and_then(|meta| meta.get("preservedMessages"))
        .and_then(|preserved| preserved.get("uuids"))
        .and_then(Value::as_array)
        .map(|uuids| {
            uuids
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A `.jsonl` path's session id: its file stem.
fn session_id_of(path: &Path) -> Option<SessionId> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(|id| SessionId(id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId(s.to_string())
    }

    /// A synthetic continuation fixture pointing at a real file on disk (so
    /// `find_parent` can scan its project directory), overriding just the
    /// fields resolution cares about on top of the shared synthetic-meta
    /// builder.
    fn continuation_meta(id: &str, source_path: PathBuf, parent_uuid: &str) -> SessionMeta {
        SessionMeta {
            continuation_of_uuid: Some(parent_uuid.to_string()),
            source_path,
            ..crate::store::test_util::meta(id, None, None)
        }
    }

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    /// Set a file's mtime deterministically, so tests can control
    /// [`find_parent`]'s newest-first candidate order rather than relying
    /// on incidental filesystem-write timing.
    fn set_mtime(path: &Path, secs: u64) {
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    /// A `compact_boundary` line claiming `uuids` as this file's own
    /// preserved (i.e. inherited-copy) set.
    fn compact_boundary_line(uuids: &[&str]) -> String {
        let joined = uuids
            .iter()
            .map(|u| format!("\"{u}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"compactMetadata\":{{\"preservedMessages\":{{\"uuids\":[{joined}]}}}}}}\n"
        )
    }

    #[test]
    fn an_older_original_beats_a_newer_inherited_copy() {
        let dir = tempfile::tempdir().unwrap();
        let original_path = write(
            dir.path(),
            "original.jsonl",
            "{\"type\":\"user\",\"uuid\":\"P1\"}\n",
        );
        let copy_content = compact_boundary_line(&["P1"]) + "{\"type\":\"user\",\"uuid\":\"P1\"}\n";
        let copy_path = write(dir.path(), "copy.jsonl", &copy_content);
        // The copy is newer, so a plain newest-first presence scan (today's
        // behavior before this round) would pick it first; classification
        // must still prefer the true original.
        set_mtime(&original_path, 1_000);
        set_mtime(&copy_path, 2_000);
        let child_path = write(dir.path(), "child.jsonl", "{\"type\":\"mode\"}\n");
        let child = continuation_meta("child", child_path, "P1");

        let store = Store::open_in_memory().unwrap();
        let mut failed = HashSet::new();
        resolve_lineage(&store, &[child], &mut failed).unwrap();

        assert_eq!(
            store.lineage_parent_ids().unwrap(),
            HashSet::from([sid("original")])
        );
    }

    #[test]
    fn a_copy_only_witness_still_resolves_as_a_fallback() {
        // No original anywhere — today's behavior (resolve to whatever
        // contains the uuid) is preserved when a copy is all there is.
        let dir = tempfile::tempdir().unwrap();
        let copy_content = compact_boundary_line(&["P1"]) + "{\"type\":\"user\",\"uuid\":\"P1\"}\n";
        write(dir.path(), "copy.jsonl", &copy_content);
        let child_path = write(dir.path(), "child.jsonl", "{\"type\":\"mode\"}\n");
        let child = continuation_meta("child", child_path, "P1");

        let store = Store::open_in_memory().unwrap();
        let mut failed = HashSet::new();
        resolve_lineage(&store, &[child], &mut failed).unwrap();

        assert_eq!(
            store.lineage_parent_ids().unwrap(),
            HashSet::from([sid("copy")])
        );
    }

    #[test]
    fn a_copy_verdict_is_correct_with_a_second_boundary_and_trailing_lines_after_the_match() {
        // Two things at once: preserved uuids accumulate across EVERY
        // compact_boundary seen so far (not just the first — a second one
        // here is what actually claims P1), and classify decides the
        // moment the needle matches without needing anything after it —
        // the trailing lines below must not change the verdict.
        let dir = tempfile::tempdir().unwrap();
        let content = compact_boundary_line(&["other-1"])
            + "{\"type\":\"user\",\"uuid\":\"other-1\"}\n"
            + &compact_boundary_line(&["P1"])
            + "{\"type\":\"user\",\"uuid\":\"P1\"}\n"
            + "{\"type\":\"assistant\",\"uuid\":\"trailing-1\"}\n"
            + "{\"type\":\"user\",\"uuid\":\"trailing-2\"}\n";
        write(dir.path(), "copy.jsonl", &content);
        let child_path = write(dir.path(), "child.jsonl", "{\"type\":\"mode\"}\n");
        let child = continuation_meta("child", child_path, "P1");

        let store = Store::open_in_memory().unwrap();
        let mut failed = HashSet::new();
        resolve_lineage(&store, &[child], &mut failed).unwrap();

        assert_eq!(
            store.lineage_parent_ids().unwrap(),
            HashSet::from([sid("copy")])
        );
    }

    #[test]
    fn a_malformed_compact_boundary_is_treated_as_no_preserved_set() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "odd.jsonl",
            "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"compactMetadata\":\"not-an-object\"}\n\
             {\"type\":\"user\",\"uuid\":\"P1\"}\n",
        );
        let child_path = write(dir.path(), "child.jsonl", "{\"type\":\"mode\"}\n");
        let child = continuation_meta("child", child_path, "P1");

        let store = Store::open_in_memory().unwrap();
        let mut failed = HashSet::new();
        resolve_lineage(&store, &[child], &mut failed).unwrap();

        assert_eq!(
            store.lineage_parent_ids().unwrap(),
            HashSet::from([sid("odd")]),
            "a broken compactMetadata shape must fall back to treating the match as an original"
        );
    }

    #[test]
    fn resolves_parent_via_streaming_scan() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "parent.jsonl",
            "{\"type\":\"user\",\"uuid\":\"P1\"}\n",
        );
        let child_path = write(dir.path(), "child.jsonl", "{\"type\":\"mode\"}\n");
        let child = continuation_meta("child", child_path, "P1");

        let store = Store::open_in_memory().unwrap();
        let mut failed = HashSet::new();
        resolve_lineage(&store, &[child], &mut failed).unwrap();

        assert!(store.has_lineage(&sid("child")).unwrap());
        assert!(store.lineage_parent_ids().unwrap().contains(&sid("parent")));
        assert!(failed.is_empty());
    }

    #[test]
    fn budget_caps_attempts_and_failed_children_are_not_retried_within_the_run() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "parent-a.jsonl",
            "{\"type\":\"user\",\"uuid\":\"PA\"}\n",
        );
        write(
            dir.path(),
            "parent-b.jsonl",
            "{\"type\":\"user\",\"uuid\":\"PB\"}\n",
        );
        let fail_path = write(dir.path(), "child-fail.jsonl", "{\"type\":\"mode\"}\n");
        let a_path = write(dir.path(), "child-a.jsonl", "{\"type\":\"mode\"}\n");
        let b_path = write(dir.path(), "child-b.jsonl", "{\"type\":\"mode\"}\n");

        // 3 pending unresolved children: one unresolvable (no file anywhere
        // carries its uuid), two resolvable. Order matters: child-fail is
        // attempted (and fails) before the two resolvable ones, so a
        // budget of 2 spends its first slot on the failure and only
        // resolves one of the two resolvable children in this pass.
        let sessions = vec![
            continuation_meta("child-fail", fail_path, "does-not-exist"),
            continuation_meta("child-a", a_path, "PA"),
            continuation_meta("child-b", b_path, "PB"),
        ];

        let store = Store::open_in_memory().unwrap();
        let mut failed = HashSet::new();
        resolve_lineage(&store, &sessions, &mut failed).unwrap();

        assert!(failed.contains(&sid("child-fail")));
        assert!(store.has_lineage(&sid("child-a")).unwrap());
        assert!(!store.has_lineage(&sid("child-b")).unwrap());

        // A second pass must not retry child-fail (still in `failed`) or
        // re-spend budget on the already-resolved child-a, so its one
        // budget slot goes to child-b.
        resolve_lineage(&store, &sessions, &mut failed).unwrap();
        assert!(store.has_lineage(&sid("child-b")).unwrap());
        assert_eq!(failed.len(), 1, "child-fail must not be retried this run");
    }

    #[test]
    fn a_scan_that_finds_nothing_leaves_no_lineage_row() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "unrelated.jsonl", "{\"type\":\"mode\"}\n");
        let child_path = write(dir.path(), "child.jsonl", "{\"type\":\"mode\"}\n");
        let child = continuation_meta("child", child_path, "nowhere-to-be-found");

        let store = Store::open_in_memory().unwrap();
        let mut failed = HashSet::new();
        resolve_lineage(&store, &[child], &mut failed).unwrap();

        assert!(!store.has_lineage(&sid("child")).unwrap());
        assert!(failed.contains(&sid("child")));
    }

    #[test]
    fn sessions_without_a_continuation_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let child_path = write(dir.path(), "child.jsonl", "{\"type\":\"mode\"}\n");
        let plain = SessionMeta {
            continuation_of_uuid: None,
            source_path: child_path,
            ..crate::store::test_util::meta("child", None, None)
        };

        let store = Store::open_in_memory().unwrap();
        let mut failed = HashSet::new();
        resolve_lineage(&store, &[plain], &mut failed).unwrap();

        assert!(!store.has_lineage(&sid("child")).unwrap());
        assert!(failed.is_empty());
    }
}
