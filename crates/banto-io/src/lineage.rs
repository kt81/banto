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

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
/// itself), newest first, for the first one containing a record with
/// `uuid` as its `uuid` field — that file's id is the parent.
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

    let needle = format!("\"uuid\":\"{uuid}\"");
    candidates
        .into_iter()
        .find(|(_, path)| file_contains(path, &needle))
        .and_then(|(_, path)| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(|id| SessionId(id.to_string()))
        })
}

/// Streams `path` line by line looking for `needle`, never loading the
/// whole file into memory. Lenient: an unreadable file is treated as not
/// containing it.
fn file_contains(path: &Path, needle: &str) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {
                if String::from_utf8_lossy(&buf).contains(needle) {
                    return true;
                }
            }
        }
    }
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
