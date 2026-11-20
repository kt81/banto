//! Records the commit this binary was built from, for the pane lifecycle log's
//! start line. A `BANTO_BUILD_COMMIT` already in the build environment wins, cut
//! to 12 characters like git's answer: the release workflow sets it to the
//! commit it checked out, and a source archive with no `.git` has no other way to
//! know. Otherwise git is asked; `unknown` when it cannot say (no `.git`, or a
//! checkout without git on the path). Changing the variable reruns this script.
//!
//! The reflog is watched because it changes on every commit and checkout, which
//! is when the answer changes; `.git/HEAD` alone changes only on a checkout.
//! A path that does not exist cannot be watched, and cargo treats a missing
//! watched path as changed on every build, so such a path is replaced by this
//! build script itself. A worktree keeps `.git` as a file, which has no `HEAD`
//! to watch: its commit is read only when this script reruns.
//!
//! The commit is HEAD and nothing more, so a binary built from edited sources
//! names the commit the tree started from. `BANTO_BUILD_DIRTY` says whether that
//! happened: `yes` when `git status` lists a tracked change or an untracked file
//! under the sources the binary is built from (the workspace's `crates/`,
//! `Cargo.toml` and `Cargo.lock`), `no` when it lists nothing, `unknown` when
//! there is no `.git` or git fails. Files elsewhere in the repository (docs, CI,
//! scripts) are not part of the binary and do not count. Ignored files do not
//! count either.
//!
//! The flag is recomputed when anything under `crates/`, or the two manifests,
//! changes: a directory `rerun-if-changed` makes cargo compare the newest mtime
//! in the tree. That costs a walk of the source tree on every build and one
//! `git status` per rerun; nothing is compiled more often than before, since
//! the banto crate is rebuilt by those same changes anyway. Without it a clean
//! flag would outlive the edit that made the tree dirty, which is the case the
//! flag exists for. An edit reverted by hand still touches its file, so the
//! script reruns and finds the tree clean again. One case lags on Windows:
//! deleting a tracked file and changing nothing else was noticed by the build
//! after the next one, not the next (measured on a synthetic tree; on Linux it is
//! noticed at once), so a tree dirty only by a deletion can name itself clean for
//! one build. `git status` runs with
//! optional locks off so a build never rewrites the index under an operator's
//! own git.
//!
//! Git's answers, the commit and the flag, come from the repository two levels up
//! and from no other: git is asked only where `../../.git` exists, because a
//! source archive unpacked inside some other checkout would otherwise report that
//! checkout's state. The flag is never taken from the environment: a commit the
//! release workflow names comes with a clean checkout's `no`, and with no `.git`
//! the flag is `unknown`.

use std::path::Path;
use std::process::Command;

/// What `git status` is asked about: the sources the binary is built from.
const SOURCES: [&str; 3] = ["crates", "Cargo.toml", "Cargo.lock"];

fn main() {
    let in_repo = Path::new("../../.git").exists();
    println!("cargo:rerun-if-env-changed=BANTO_BUILD_COMMIT");
    let commit = provided_commit()
        .or_else(|| if in_repo { head_commit() } else { None })
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = if in_repo { is_dirty() } else { None };
    println!("cargo:rustc-env=BANTO_BUILD_COMMIT={commit}");
    println!(
        "cargo:rustc-env=BANTO_BUILD_DIRTY={}",
        match dirty {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown",
        }
    );
    for path in ["../../.git/HEAD", "../../.git/logs/HEAD"] {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        } else {
            println!("cargo:rerun-if-changed=build.rs");
        }
    }
    for source in SOURCES {
        println!("cargo:rerun-if-changed=../../{source}");
    }
}

/// The commit the environment names, as 12 characters; `None` when the variable
/// is unset or blank.
fn provided_commit() -> Option<String> {
    let value = std::env::var("BANTO_BUILD_COMMIT").ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.chars().take(12).collect())
}

fn head_commit() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Whether the sources differ from HEAD, tracked or untracked; `None` when git
/// cannot say. An empty listing is clean, and only a successful run may say so.
fn is_dirty() -> Option<bool> {
    Command::new("git")
        .args(["-C", "../..", "status", "--porcelain", "--"])
        .args(SOURCES)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.iter().all(u8::is_ascii_whitespace))
}
