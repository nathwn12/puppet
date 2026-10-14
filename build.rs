//! Build-time provenance stamp: the exact commit this binary was built from.
//!
//! The repo has no CI, so a deployed `puppet.exe` has no other way to say where
//! it came from; `puppet --version` prints these values, which makes binary
//! currency and dirty-tree builds re-measurable against `git rev-parse HEAD`.
//!
//! The rerun set below is explicit rather than left to cargo's "any file in the
//! package changed" default, because a `git commit` moves HEAD WITHOUT touching
//! any working-tree mtime - the default would then reuse the pre-commit stamp
//! and a fresh binary would report a stale sha.
//!
//! The watched paths are the ones git actually rewrites on a HEAD move. For a
//! commit on a branch that is the loose ref (`.git/refs/heads/<branch>`) and the
//! reflog (`.git/logs/HEAD`); `.git/index` and `.git/HEAD` are NOT written (both
//! measured against `git commit --allow-empty`, 2026-10-14). A checkout or a
//! detached-HEAD commit rewrites `.git/HEAD`, and a repository with reflogs
//! disabled still moves `.git/refs/heads`, so the two sets together cover every
//! HEAD move; a path that is absent (git worktree, source tarball) is treated by
//! cargo as dirty, which rebuilds the stamp rather than reusing a stale one.

use std::process::Command;

/// Run `git` in the package root. `None` when git is missing or the command
/// fails (source tarball, no git on PATH): an unstamped build is degraded, not
/// broken, so this never fails the build.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let sha = git(&["rev-parse", "HEAD"])
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());

    println!("cargo:rustc-env=PUPPET_GIT_SHA={sha}");
    // Empty, or the suffix appended to the sha in the version string.
    println!(
        "cargo:rustc-env=PUPPET_GIT_DIRTY={}",
        if dirty { "-dirty" } else { "" }
    );

    // Re-stamp on a commit (loose ref + reflog), a checkout or detached-HEAD
    // move (`.git/HEAD`), a staged edit (index), an unstaged source edit (src/,
    // tests/), or a manifest edit. Nothing else can change the stamp, so
    // `target/` and `bin/` are deliberately not walked.
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=.git/logs/HEAD");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=tests");
}
