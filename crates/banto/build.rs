//! Records the commit this binary was built from, for the pane lifecycle log's
//! start line. `unknown` when git cannot say (a source archive, or a checkout
//! without git on the path).
//!
//! The reflog is watched because it changes on every commit and checkout, which
//! is when the answer changes; `.git/HEAD` alone changes only on a checkout.
//! A path that does not exist cannot be watched, and cargo treats a missing
//! watched path as changed on every build, so such a path is replaced by this
//! build script itself. A worktree keeps `.git` as a file, which has no `HEAD`
//! to watch: its commit is read only when this script reruns.
//!
//! The commit is HEAD and nothing more: uncommitted edits are not recorded, so a
//! binary built from a dirty tree names the commit the tree started from. There
//! is no dirty flag.

use std::path::Path;
use std::process::Command;

fn main() {
    let commit = if Path::new("../../.git").exists() {
        head_commit()
    } else {
        None
    }
    .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=BANTO_BUILD_COMMIT={commit}");
    for path in ["../../.git/HEAD", "../../.git/logs/HEAD"] {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        } else {
            println!("cargo:rerun-if-changed=build.rs");
        }
    }
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
