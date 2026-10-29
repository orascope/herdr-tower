// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Stamps version, commit and build date into the binary.
//!
//! Cargo records neither the commit a binary was built from nor a version a
//! release pipeline wants to stamp on it, so that information is gathered here
//! and handed over as environment variables the crate reads with `env!`.
//!
//! The precedence is deliberate: an explicitly injected value always
//! wins, and git is only consulted for what was left blank. A release build sets
//! the variables and never touches git; a developer's `cargo build` sets none
//! and gets the working tree's actual state.

use std::process::Command;

fn main() {
    // A tag from the release pipeline, else nothing — *not* CARGO_PKG_VERSION.
    // Cargo.toml carries 0.1.0 for every crate in this workspace, and reporting
    // that as the product version would be worse than reporting none: `version`
    // is how a bug report says which build it came from.
    let injected_version = env("HERDR_TOWER_VERSION");
    let mut version = injected_version.clone();
    if version.is_empty() {
        // 🚨 Asked of git, like the commit beside it. A build that reports no
        // version at all shows as `dev` everywhere it matters — the tower's
        // header, `version`, the supervisor's status — and a person looking at
        // a popup cannot tell which of the day's builds they are running.
        // `describe` answers that: `v0.1.12-5-gabc1234` names the last tag and
        // how far past it this is.
        //
        version = git(&["describe", "--tags", "--always"]);
    }
    let mut commit = env("HERDR_TOWER_COMMIT");
    let date = env("HERDR_TOWER_DATE");

    // Whether the commit came from git decides whether `+dirty` may be added:
    // a pipeline that named a commit is describing a tag, and
    // appending the state of whatever tree happened to build it would be a lie.
    let commit_from_git = commit.is_empty();
    if commit_from_git {
        commit = git(&["rev-parse", "HEAD"]);
    }
    let dirty = commit_from_git && !commit.is_empty() && tracked_files_modified();
    if dirty {
        commit.push_str("+dirty");
        // 🚨 And on the version, not only the commit. The compact surfaces —
        // the tower header, the daemon snapshot — show `version_label` alone,
        // so without this a tree with edits built on a release tag presents
        // itself as that exact release. Being unable to tell a modified binary
        // from a shipped one is the failure this whole stamp exists to prevent.
        //
        // Only for a version git supplied. An injected one is the pipeline
        // describing a tag, and appending the state of whatever tree happened
        // to build it would be a lie — the same rule the commit follows above.
        if injected_version.is_empty() && !version.is_empty() {
            version.push_str("+dirty");
        }
    }

    let date = if date.is_empty() {
        // The commit's own time, not the moment of compilation: two builds of
        // one commit should not disagree, and "when was this code written" is
        // the more useful answer in a bug report anyway.
        git(&["log", "-1", "--format=%cI"])
    } else {
        date
    };

    // The compiler that produced this binary. Rust cannot learn its own
    // compiler version at run time, so it has to be captured here or not
    // reported at all. It is the half of the
    // platform line that answers "why does this binary not run there".
    let rustc = {
        let full = git_free(
            &std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()),
            &["--version"],
        );
        // "rustc 1.93.0 (abc123 2026-08-01)" -> "1.93.0"
        full.split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string()
    };

    println!("cargo:rustc-env=AL_RUSTC={rustc}");
    println!("cargo:rustc-env=AL_VERSION={version}");
    println!("cargo:rustc-env=AL_COMMIT={commit}");
    println!("cargo:rustc-env=AL_DATE={date}");

    // Without these, cargo reruns this script on every build of every file.
    for k in [
        "HERDR_TOWER_VERSION",
        "HERDR_TOWER_COMMIT",
        "HERDR_TOWER_DATE",
    ] {
        println!("cargo:rerun-if-env-changed={k}");
    }
    // A checkout or a change to the index moves HEAD or the index file.
    // `--git-path` rather than a hardcoded `../../.git/…` because it resolves the
    // indirection in a linked worktree, where `.git` is a file and the real HEAD
    // lives elsewhere. Outside a repository — a release tarball — git answers
    // nothing and nothing is watched.
    for p in ["HEAD", "index"] {
        let resolved = git(&["rev-parse", "--git-path", p]);
        if !resolved.is_empty() {
            println!("cargo:rerun-if-changed={resolved}");
        }
    }
    watch_refs();
    watch_tracked_files();
}

/// Declares the refs `git describe` reads as build inputs.
///
/// HEAD and the index are not enough. HEAD is a symbolic ref: a commit moves
/// the branch it names and leaves HEAD itself as it was. And tagging a commit
/// that was already built moves nothing at all among the files above, so the
/// build script kept its earlier answer and `make install` on the release
/// commit installed a binary still calling itself `v0.1.12-15-g…` rather than
/// the tag it had become — the exact-version check a release is held to fails
/// on a build that is otherwise correct.
///
/// Only paths that exist. Cargo treats a `rerun-if-changed` on something
/// absent as permanently stale and reruns on every build. That is not a loss
/// here: `packed-refs` appears when loose refs are packed, which removes them
/// from `refs/tags` and so changes a watched directory in the same step.
fn watch_refs() {
    let mut refs = vec!["refs/tags".to_string(), "packed-refs".to_string()];
    // The branch HEAD names, if it names one. A detached HEAD holds the commit
    // itself, and the HEAD file already watched above is that answer.
    let branch = git(&["symbolic-ref", "-q", "HEAD"]);
    if !branch.is_empty() {
        refs.push(branch);
    }
    for r in refs {
        let resolved = git(&["rev-parse", "--git-path", &r]);
        if !resolved.is_empty() && std::path::Path::new(&resolved).exists() {
            println!("cargo:rerun-if-changed={resolved}");
        }
    }
}

/// Whether any *tracked* file differs from HEAD.
///
/// Untracked files are deliberately not dirt. They are not inputs to the build,
/// a scratch note beside the source does not change the code that is running,
/// and — the load-bearing half — the set this question is asked about has to be
/// the same set [`watch_tracked_files`] watches. Counting `??` entries would ask
/// about files cargo cannot be told to watch, which is how the marker went stale.
fn tracked_files_modified() -> bool {
    !git(&["status", "--porcelain", "--untracked-files=no"]).is_empty()
}

/// Declares every tracked file a build input, so the `+dirty` marker cannot go
/// stale.
///
/// Without this, cargo had only HEAD and the index to invalidate on, and neither
/// moves when a file is edited without being staged: the build script kept its
/// previous answer and the binary reported a clean commit while the tree that
/// produced it was modified. `short_commit` in `versioncmd.rs` goes out of its
/// way to preserve this marker precisely because the version string must not lie
/// about which code is running, and a stale marker is that same lie arriving by a
/// slower route.
///
/// The cost is paid in the other direction: editing any tracked file — a README,
/// a workflow — now reruns this script and relinks the binary. That is the trade,
/// and it is the right way round.
fn watch_tracked_files() {
    // `-z` because a path may contain anything but NUL, and git quotes such paths
    // in its default output. Run from the crate directory, `ls-files` would list
    // only this crate's files, so it is anchored at the top level and the paths
    // are rejoined there.
    let root = git(&["rev-parse", "--show-toplevel"]);
    if root.is_empty() {
        return;
    }
    let out = match git_command().args(["-C", &root, "ls-files", "-z"]).output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return,
    };
    for f in out.split(|&b| b == 0).filter(|f| !f.is_empty()) {
        // A path that is not UTF-8 cannot be handed to cargo through a println,
        // and no file in this repository has one; skipping is the honest failure.
        if let Ok(rel) = std::str::from_utf8(f) {
            println!("cargo:rerun-if-changed={root}/{rel}");
        }
    }
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// Runs git, or yields an empty string.
///
/// Every failure is the same answer on purpose. A build from a release tarball
/// has no `.git`, a build in a sandbox may have no `git` at all, and neither is
/// a reason to fail a compile — the version line simply says less.
fn git(args: &[&str]) -> String {
    trimmed_output(git_command().args(args))
}

/// `git` without git's own environment.
///
/// 🚨 The stamp describes the tree being built, which is the one this script
/// runs in. A `GIT_DIR` inherited from a hook, or set by hand, overrides that:
/// the version came out `dev` (no repository there) or another repository's,
/// and cargo kept the result, because nothing tells it the stamp depends on
/// those variables — a later build in a clean shell still said `dev`
/// (`a_local_build_is_not_nameless`, 2026-09-27). Stripped, the answer only
/// depends on the directory, which cargo already watches.
fn git_command() -> Command {
    let mut cmd = Command::new("git");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(k);
        }
    }
    cmd
}

/// Runs any command for its trimmed output, or yields an empty string.
fn git_free(bin: &str, args: &[&str]) -> String {
    trimmed_output(Command::new(bin).args(args))
}

fn trimmed_output(cmd: &mut Command) -> String {
    cmd.output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}
