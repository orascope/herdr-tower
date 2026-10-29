// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Every git command this crate runs, gathered where one test can read them all.

use std::process::Command;

use crate::{Error, Result};

/// One git invocation, as a value rather than as a call site.
///
/// The argv *is* the safety contract, and three tokens in this table are the
/// entire protection on the destructive paths: `worktree remove` **without**
/// `--force` keeps git's own refusal to discard a dirty worktree, `branch -d`
/// (never `-D`) keeps an unmerged branch alive, and `merge --no-ff` keeps a
/// task's commits legible as a branch after it lands. Scattered across a dozen
/// `Command::new("git")` calls, a `--force` slipping in is a one-token diff
/// nobody reads twice; collected here it is a one-token diff in a table that
/// [`tests::the_argv_of_every_git_command`] asserts wholesale.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Op<'a> {
    /// The repository root containing this directory.
    RepoRoot,
    /// The shared `.git` directory — the one holding `info/exclude`, which a
    /// linked worktree does not have of its own.
    GitCommonDir,
    /// The branch HEAD is on, or empty output when HEAD is detached.
    CurrentBranch,
    /// The commit HEAD points at.
    Head,
    /// Whether a local branch of that name exists.
    VerifyBranch(&'a str),
    /// A new branch cut from `base`, checked out into its own worktree.
    AddWorktree {
        base: &'a str,
        branch: &'a str,
        path: &'a str,
    },
    /// Uncommitted and untracked files, one per line.
    Status,
    /// How many commits `branch` has that `base` does not.
    CountRange {
        base: &'a str,
        branch: &'a str,
    },
    /// How many commits either branch has that `base` does not, each
    /// counted once.
    CountUnion {
        base: &'a str,
        branches: [&'a str; 2],
    },
    /// Everything that differs from `base`, committed and working tree alike.
    Diff(&'a str),
    /// Untracked files that are not ignored.
    Untracked,
    /// Detach a worktree. No `--force`: git refuses a dirty one and that
    /// refusal is load-bearing.
    RemoveWorktree(&'a str),
    /// Forget worktree administrative files left behind by a removal.
    PruneWorktrees,
    /// Delete a branch, but only a merged one (`-d`, never `-D`).
    DeleteBranch(&'a str),
    Checkout(&'a str),
    /// Merge with a merge commit even when a fast-forward is possible.
    MergeNoFf(&'a str),
}

impl Op<'_> {
    pub(crate) fn args(&self) -> Vec<String> {
        let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        match *self {
            Op::RepoRoot => owned(&["rev-parse", "--show-toplevel"]),
            Op::Head => owned(&["rev-parse", "HEAD"]),
            Op::CurrentBranch => owned(&["branch", "--show-current"]),
            Op::VerifyBranch(b) => vec![
                "rev-parse".into(),
                "--verify".into(),
                format!("refs/heads/{b}"),
            ],
            Op::AddWorktree { base, branch, path } => vec![
                "worktree".into(),
                "add".into(),
                "-b".into(),
                branch.into(),
                path.into(),
                base.into(),
            ],
            Op::GitCommonDir => owned(&["rev-parse", "--path-format=absolute", "--git-common-dir"]),
            Op::Status => owned(&["status", "--porcelain", "-uall"]),
            Op::CountRange { base, branch } => {
                vec![
                    "rev-list".into(),
                    "--count".into(),
                    format!("{base}..{branch}"),
                ]
            }
            Op::CountUnion { base, branches } => vec![
                "rev-list".into(),
                "--count".into(),
                format!("^{base}"),
                branches[0].into(),
                branches[1].into(),
                "--".into(),
            ],
            Op::Diff(base) => vec!["diff".into(), base.into()],
            Op::Untracked => owned(&["ls-files", "--others", "--exclude-standard"]),
            Op::RemoveWorktree(p) => vec!["worktree".into(), "remove".into(), p.into()],
            Op::PruneWorktrees => owned(&["worktree", "prune"]),
            Op::DeleteBranch(b) => vec!["branch".into(), "-d".into(), b.into()],
            Op::Checkout(b) => vec!["checkout".into(), b.into()],
            Op::MergeNoFf(b) => vec![
                "merge".into(),
                "--no-ff".into(),
                b.into(),
                "-m".into(),
                format!("Merge {b} (herdr-tower wt)"),
            ],
        }
    }
}

/// Runs `git -C <dir> <op>` and returns its trimmed standard output.
///
/// The two streams are treated differently on success and on failure:
///
///   - **On success only stdout is the value.** Merging stderr into a parsed
///     result means any warning git decides to print becomes data: one line on
///     stderr during [`dirty`] is a phantom uncommitted file, and one during
///     [`unmerged`] makes the count unparseable. The failure mode is a `wt
///     clean` that refuses forever with nothing actually dirty.
///   - **On failure both streams go into the message**, because that is where
///     git's explanation lives. Note the ordering is stdout-then-stderr rather
///     than the child's own interleaving; a single shared pipe is not something
///     `std::process` offers.
///
/// A git that cannot be spawned at all also reports through [`Error::Git`], with
/// the OS error where the output would be — otherwise `git` missing from `PATH`
/// would produce a message with nothing after the colon.
/// The variables that tell git which repository it is in.
///
/// Set, they override `-C`: git works on the repository they name, not on the
/// directory it was pointed at. Git sets them itself when it runs a hook, so
/// anything started from one — a `pre-push` running the test suite — inherits
/// them. This is the same class of variable git clears before it runs a
/// command in a different repository.
pub(crate) const REPO_LOCATING_ENV: [&str; 16] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_PREFIX",
    "GIT_NAMESPACE",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// A `git` that works on the repository it is pointed at, whatever repository
/// the caller happens to be inside.
///
/// 🚨 Every git this crate runs goes through here. When the pre-push hook first
/// ran the test suite, git had exported `GIT_DIR` for the hook, and each
/// fixture's `git -C <tempdir> init`, `config` and `commit` landed in the real
/// repository instead: its shared config gained `core.bare = true` — which
/// broke the main checkout — and a `user.email` of `fixture@example.invalid`,
/// the branch being pushed gained a commit deleting every file, and `wt new`'s
/// tests registered worktrees and branches in it. The product is exposed the
/// same way: `herdr-tower wt …` started from a hook would act on the wrong
/// repository.
pub(crate) fn git() -> Command {
    let mut cmd = Command::new("git");
    for var in REPO_LOCATING_ENV {
        cmd.env_remove(var);
    }
    cmd
}

fn run(dir: &str, op: Op) -> Result<String> {
    let args = op.args();
    let out = match git().arg("-C").arg(dir).args(&args).output() {
        Ok(out) => out,
        Err(e) => {
            return Err(Error::Git {
                args: args.join(" "),
                output: e.to_string(),
            })
        }
    };
    if !out.status.success() {
        let mut both = String::from_utf8_lossy(&out.stdout).into_owned();
        both.push_str(&String::from_utf8_lossy(&out.stderr));
        return Err(Error::Git {
            args: args.join(" "),
            output: both.trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The repository root `dir` belongs to.
pub fn repo_root(dir: &str) -> Result<String> {
    // git prints `C:/…` on Windows; the task records hold `C:\…`.
    run(dir, Op::RepoRoot).map(|p| tower_os::native_path(&p))
}

/// Keeps the generated worktrees out of the main checkout's `git status`.
///
/// `wt new` puts a populated worktree at `<repo>/.herdr-tower/worktrees/<task>`,
/// and a repository that does not already ignore that path reports the whole
/// worker tree as untracked — where a later `git add .` can stage it.
///
/// `.git/info/exclude` rather than `.gitignore`: the rule is this machine's, not
/// the project's, and a tool that edits a tracked file to make its own mess
/// invisible is a tool that shows up in somebody's diff. Idempotent, and
/// best-effort — a repository that will not take the line still gets its
/// worktree, it just also gets the noise.
pub(crate) fn ignore_generated_worktrees(repo: &str) -> Result<()> {
    const RULE: &str = "/.herdr-tower/";
    let common = tower_os::native_path(&run(repo, Op::GitCommonDir)?);
    if common.is_empty() {
        return Ok(());
    }
    let info = std::path::Path::new(&common).join("info");
    let path = info.join("exclude");
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    if body.lines().any(|l| l.trim() == RULE) {
        return Ok(());
    }
    std::fs::create_dir_all(&info)?;
    let mut next = body;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&format!("{RULE}\n"));
    std::fs::write(&path, next)?;
    Ok(())
}

/// The branch HEAD is on. Empty means detached, which is a fact rather than a
/// failure and is why this does not error on it.
///
/// Public because a drive has to ask it of a *worktree*: that tree can be moved
/// off its task branch — by the worker, or by someone bisecting in it — and
/// then what passed the tests is not what `wt merge` would merge, which goes by
/// the branch recorded on the task.
pub fn current_branch(repo: &str) -> Result<String> {
    run(repo, Op::CurrentBranch)
}

/// The branch checked out in the worktree at `path`, read from its `HEAD`
/// file rather than by running git.
///
/// For the places that *show* a branch — the tower's rows, herdr's sidebar,
/// the Discord card, a preview window's title — on every pass: a git process
/// per folder costs ~17 ms, reading one small file costs microseconds. A
/// worker often moves its worktree to a branch of its own
/// (`agent/i-2919` → `agent/explain-2969-stage2`), and the branch in the task
/// record never hears of it; this is where the work actually is.
///
/// `None` for a detached HEAD and for anything that is not a checkout —
/// callers fall back to the recorded branch.
pub fn checked_out_branch(path: &str) -> Option<String> {
    let dot_git = std::path::Path::new(path).join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // A linked worktree's `.git` is a file: `gitdir: <path>`, relative to
        // the worktree when it is not absolute.
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let target = pointer.trim().strip_prefix("gitdir:")?.trim();
        std::path::Path::new(path).join(target)
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_string())
}

/// Whether a local branch of that name exists.
///
/// A bool and not a `Result`: git failing to reach the repository and the branch
/// being absent both mean "you may not use this name", and the one caller
/// ([`crate::new_task`]) has already established the repository exists.
pub fn branch_exists(repo: &str, branch: &str) -> bool {
    run(repo, Op::VerifyBranch(branch)).is_ok()
}

/// Cuts `branch` from `base` and checks it out into a new worktree at `path`.
pub fn add_worktree(repo: &str, base: &str, branch: &str, path: &str) -> Result<()> {
    run(repo, Op::AddWorktree { base, branch, path }).map(|_| ())
}

/// The commit a worktree's HEAD points at.
///
/// A verdict is about a tree, and a tree is a commit: without this, a drive's
/// `merge-ready` keeps standing after the worker is prompted directly, commits
/// again, or amends the branch — the tree stays clean, the column keeps
/// advertising, and what would merge is not what was tested.
pub fn head(path: &str) -> Result<String> {
    run(path, Op::Head).map(|out| out.trim().to_string())
}

/// The worktree's uncommitted and untracked files, one entry per porcelain line.
pub fn dirty(path: &str) -> Result<Vec<String>> {
    let out = run(path, Op::Status)?;
    if out.is_empty() {
        return Ok(Vec::new());
    }
    Ok(out.lines().map(|l| l.to_string()).collect())
}

/// How many commits on `branch` have not landed on `base`.
pub fn unmerged(repo: &str, base: &str, branch: &str) -> Result<usize> {
    let out = run(repo, Op::CountRange { base, branch })?;
    out.parse().map_err(|_| Error::Count { output: out })
}

/// How many commits on `a` or `b` have not landed on `base`, a commit on both
/// counted once — a branch cut from the other shares its commits.
pub fn unmerged_either(repo: &str, base: &str, a: &str, b: &str) -> Result<usize> {
    let out = run(
        repo,
        Op::CountUnion {
            base,
            branches: [a, b],
        },
    )?;
    out.parse().map_err(|_| Error::Count { output: out })
}

/// Everything the worktree changed against `base` — commits and working tree
/// both — with untracked files listed after it as comments.
///
/// The untracked listing is appended rather than merged in because there is no
/// diff to show for a file git has never seen; naming it is the most that can be
/// said, and a `#` comment is the one thing a diff reader will not mistake for
/// content.
pub fn diff(path: &str, base: &str) -> Result<String> {
    let mut diff = run(path, Op::Diff(base))?;
    // A failure here is swallowed on purpose: the committed diff is the answer,
    // and losing the untracked footnote must not lose it.
    if let Ok(out) = run(path, Op::Untracked) {
        if !out.is_empty() {
            diff.push_str("\n# untracked files:\n");
            for f in out.lines() {
                diff.push_str("#   ");
                diff.push_str(f);
                diff.push('\n');
            }
        }
    }
    Ok(diff)
}

/// Detaches the worktree at `path` and prunes what the removal left behind.
///
/// The safety check belongs to the caller ([`crate::clean`]) — but not
/// exclusively, which is the point of never passing `--force`: git's own refusal
/// to remove a dirty worktree stays underneath ours as a second line of defence.
pub fn remove_worktree(repo: &str, path: &str) -> Result<()> {
    run(repo, Op::RemoveWorktree(path))?;
    // Best effort: a failed prune leaves administrative files, not lost work.
    let _ = run(repo, Op::PruneWorktrees);
    Ok(())
}

/// Deletes a merged branch. An unmerged one makes git refuse, which is wanted.
pub fn delete_branch(repo: &str, branch: &str) -> Result<()> {
    run(repo, Op::DeleteBranch(branch)).map(|_| ())
}

/// Checks out `base` in `repo` and merges `branch` into it with a merge commit.
///
/// Assumes the confirmation in [`crate::merge_guide`] already happened. On
/// conflict git's own message comes back untouched — this is the point where a
/// person has to take over, and paraphrasing git would only hide the resolution
/// instructions it already printed.
pub fn merge(repo: &str, base: &str, branch: &str) -> Result<()> {
    run(repo, Op::Checkout(base))?;
    run(repo, Op::MergeNoFf(branch)).map(|_| ())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    /// The whole table at once. This is the test that has to fail when somebody
    /// adds `--force` to a removal or turns `-d` into `-D`.
    #[test]
    fn the_argv_of_every_git_command() {
        let cases: Vec<(Op, Vec<&str>)> = vec![
            (Op::RepoRoot, vec!["rev-parse", "--show-toplevel"]),
            (Op::Head, vec!["rev-parse", "HEAD"]),
            (Op::CurrentBranch, vec!["branch", "--show-current"]),
            (
                Op::VerifyBranch("agent/t1"),
                vec!["rev-parse", "--verify", "refs/heads/agent/t1"],
            ),
            (
                Op::AddWorktree {
                    base: "main",
                    branch: "agent/t1",
                    path: "/w/t1",
                },
                vec!["worktree", "add", "-b", "agent/t1", "/w/t1", "main"],
            ),
            (Op::Status, vec!["status", "--porcelain", "-uall"]),
            (
                Op::CountRange {
                    base: "main",
                    branch: "agent/t1",
                },
                vec!["rev-list", "--count", "main..agent/t1"],
            ),
            (
                Op::CountUnion {
                    base: "main",
                    branches: ["agent/t1", "agent/x"],
                },
                vec!["rev-list", "--count", "^main", "agent/t1", "agent/x", "--"],
            ),
            (Op::Diff("main"), vec!["diff", "main"]),
            (
                Op::Untracked,
                vec!["ls-files", "--others", "--exclude-standard"],
            ),
            (
                Op::RemoveWorktree("/w/t1"),
                vec!["worktree", "remove", "/w/t1"],
            ),
            (Op::PruneWorktrees, vec!["worktree", "prune"]),
            (
                Op::DeleteBranch("agent/t1"),
                vec!["branch", "-d", "agent/t1"],
            ),
            (Op::Checkout("main"), vec!["checkout", "main"]),
            (
                Op::MergeNoFf("agent/t1"),
                vec![
                    "merge",
                    "--no-ff",
                    "agent/t1",
                    "-m",
                    "Merge agent/t1 (herdr-tower wt)",
                ],
            ),
        ];
        for (op, want) in cases {
            assert_eq!(op.args(), want, "argv for {op:?}");
        }
    }

    /// Spelled out separately from the table because these three tokens are the
    /// only thing standing between `wt clean` and somebody's uncommitted work.
    #[test]
    fn destructive_commands_never_carry_a_force_flag() {
        for op in [
            Op::RemoveWorktree("/w"),
            Op::DeleteBranch("b"),
            Op::MergeNoFf("b"),
        ] {
            let args = op.args();
            assert!(
                !args
                    .iter()
                    .any(|a| a == "--force" || a == "-f" || a == "-D"),
                "{op:?} must not force: {args:?}"
            );
        }
    }

    /// Every variable that can point git at another repository is removed from
    /// the git this crate runs — whatever the caller's environment holds.
    #[test]
    fn a_git_from_here_cannot_be_pointed_at_another_repository() {
        let cmd = super::git();
        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for var in super::REPO_LOCATING_ENV {
            assert!(removed.iter().any(|r| r == var), "{var} is not cleared");
        }
        assert!(super::REPO_LOCATING_ENV.contains(&"GIT_DIR"));
        assert!(super::REPO_LOCATING_ENV.contains(&"GIT_WORK_TREE"));
        assert!(super::REPO_LOCATING_ENV.contains(&"GIT_INDEX_FILE"));
    }

    /// 🚨 And nothing in this crate runs git any other way. A single spawn that
    /// bypasses `git()` is enough: under a hook's `GIT_DIR` it is the one that
    /// writes into the real repository — which is how a new test fixture would
    /// bring this back.
    #[test]
    fn every_git_this_crate_runs_goes_through_the_scrubbed_constructor() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut spawns = Vec::new();
        for entry in std::fs::read_dir(&src).expect("src") {
            let path = entry.expect("entry").path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read");
            for (n, line) in text.lines().enumerate() {
                let code = line.trim_start();
                if !code.starts_with("//")
                    && code.contains(concat!("Command::new(", "\"git\"", ")"))
                {
                    spawns.push(format!(
                        "{}:{}",
                        path.file_name().unwrap().to_string_lossy(),
                        n + 1
                    ));
                }
            }
        }
        assert_eq!(
            spawns.len(),
            1,
            "only `git()` itself may spawn git directly: {spawns:?}"
        );
        assert!(spawns[0].starts_with("git.rs:"), "{spawns:?}");
    }

    pub(crate) fn detach(dir: &str) {
        git_run(Path::new(dir), &["switch", "--detach"]);
    }

    pub(crate) fn switch_new(dir: &str, branch: &str) {
        git_run(Path::new(dir), &["switch", "-c", branch]);
    }

    /// The branch is read from `HEAD` itself, in a main checkout and in a
    /// linked worktree alike, and follows a switch — which is the case the
    /// recorded branch misses.
    #[test]
    fn the_checked_out_branch_is_read_from_head_and_follows_a_switch() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap().to_string();
        assert_eq!(checked_out_branch(&repo_path).as_deref(), Some("main"));

        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("i-2919");
        let wt_path = wt.to_str().unwrap().to_string();
        add_worktree(&repo_path, "main", "agent/i-2919", &wt_path).unwrap();
        assert_eq!(
            checked_out_branch(&wt_path).as_deref(),
            Some("agent/i-2919")
        );

        switch_new(&wt_path, "agent/explain-2969-stage2");
        assert_eq!(
            checked_out_branch(&wt_path).as_deref(),
            Some("agent/explain-2969-stage2")
        );
        assert_eq!(
            checked_out_branch(&wt_path),
            current_branch(&wt_path).ok(),
            "the file and git agree"
        );

        git_run(&wt, &["switch", "--detach"]);
        assert_eq!(checked_out_branch(&wt_path), None, "detached is no branch");
        assert_eq!(checked_out_branch(holder.path().to_str().unwrap()), None);
        assert_eq!(checked_out_branch("/nonexistent/x"), None);
    }

    /// Every shape a checkout's `.git` can take, built by hand: a directory,
    /// a `gitdir:` file with an absolute or a relative target, and the HEAD
    /// contents that are not a local branch.
    #[test]
    fn checked_out_branch_reads_every_git_pointer_shape() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        let head = |body: &str| std::fs::write(store.join("HEAD"), body).unwrap();
        let wt = d.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_s = wt.to_str().unwrap();

        // (name, .git pointer body, HEAD body, expected)
        let cases: &[(&str, &str, &str, Option<&str>)] = &[
            (
                "relative pointer",
                "gitdir: ../store\n",
                "ref: refs/heads/feat/x\n",
                Some("feat/x"),
            ),
            (
                "pointer without newline",
                "gitdir: ../store",
                "ref: refs/heads/main",
                Some("main"),
            ),
            (
                "CRLF and padding",
                "gitdir:   ../store  \r\n",
                "ref: refs/heads/fix/2955-a\r\n",
                Some("fix/2955-a"),
            ),
            (
                "unicode branch",
                "gitdir: ../store\n",
                "ref: refs/heads/기능/한글-1\n",
                Some("기능/한글-1"),
            ),
            (
                "detached",
                "gitdir: ../store\n",
                "3c9fecb0000000000000000000000000000000000\n",
                None,
            ),
            (
                "empty ref",
                "gitdir: ../store\n",
                "ref: refs/heads/\n",
                None,
            ),
            (
                "not a local branch",
                "gitdir: ../store\n",
                "ref: refs/remotes/origin/main\n",
                None,
            ),
            (
                "pointer without the key",
                "../store\n",
                "ref: refs/heads/main\n",
                None,
            ),
            ("empty pointer file", "", "ref: refs/heads/main\n", None),
        ];
        for (name, pointer, body, want) in cases {
            std::fs::write(wt.join(".git"), pointer).unwrap();
            head(body);
            assert_eq!(checked_out_branch(wt_s).as_deref(), *want, "{name}");
        }

        // An absolute pointer, and a pointer to a store with no HEAD.
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", store.display())).unwrap();
        head("ref: refs/heads/abs\n");
        assert_eq!(checked_out_branch(wt_s).as_deref(), Some("abs"));
        std::fs::remove_file(store.join("HEAD")).unwrap();
        assert_eq!(checked_out_branch(wt_s), None, "no HEAD behind the pointer");
    }

    /// Round trip against git itself: every branch switched to is the branch
    /// read back, and the file read agrees with `git branch --show-current`.
    #[test]
    fn checked_out_branch_agrees_with_git_for_any_branch_name() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap().to_string();
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("t");
        let wt_path = wt.to_str().unwrap().to_string();
        add_worktree(&repo_path, "main", "agent/t", &wt_path).unwrap();
        for name in [
            "a",
            "agent/i-2919",
            "fix/2955-bundle-setting",
            "기능/한글",
            "deep/a/b/c/d",
            "x.y-z_w",
            "UPPER/Case",
        ] {
            switch_new(&wt_path, name);
            let read = checked_out_branch(&wt_path);
            assert_eq!(read.as_deref(), Some(name), "read back");
            assert_eq!(read, current_branch(&wt_path).ok(), "{name}: file vs git");
        }
    }

    fn git_run(dir: &Path, args: &[&str]) {
        let out = super::git()
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repository with one commit on `main`, for the tests that need real git
    /// rather than a pinned argv.
    ///
    /// The identity is written into the repository, not just into the fixture's
    /// own commands. Product code shells out to git too — `merge` makes a commit
    /// — and it inherits whatever identity the machine has, which on a machine
    /// with none is a failure: "Committer identity unknown". That is the right
    /// answer for a person's own repository and the wrong one for a fixture, and
    /// it went unseen on every machine with a `user.email` in `~/.gitconfig`.
    /// Local to the temporary repository, so nobody's global config is touched.
    pub(crate) fn fixture_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().expect("tempdir");
        git_run(d.path(), &["init", "-b", "main"]);
        git_run(d.path(), &["config", "user.name", "fixture"]);
        git_run(
            d.path(),
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(d.path().join("a.txt"), "hello\n").expect("write");
        git_run(d.path(), &["add", "."]);
        git_run(d.path(), &["commit", "-m", "init"]);
        d
    }

    pub(crate) fn commit_all(dir: &str, message: &str) {
        git_run(Path::new(dir), &["add", "."]);
        git_run(Path::new(dir), &["commit", "-m", message]);
    }

    /// The whole lifecycle against real git, in the order a task lives it.
    #[test]
    fn a_worktree_goes_clean_dirty_committed_merged_and_removable() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap().to_string();
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("task1");
        let wt_path = wt.to_str().unwrap().to_string();

        add_worktree(&repo_path, "main", "agent/task1", &wt_path).expect("add worktree");
        assert!(branch_exists(&repo_path, "agent/task1"));
        assert!(!branch_exists(&repo_path, "agent/nope"));

        assert!(
            dirty(&wt_path).unwrap().is_empty(),
            "fresh worktree is clean"
        );
        assert_eq!(unmerged(&repo_path, "main", "agent/task1").unwrap(), 0);

        std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
        std::fs::write(wt.join("new.txt"), "new\n").unwrap();
        assert_eq!(
            dirty(&wt_path).unwrap().len(),
            2,
            "one modified, one untracked"
        );
        let d = diff(&wt_path, "main").unwrap();
        assert!(d.contains("changed"), "diff carries the modification:\n{d}");
        assert!(
            d.contains("untracked") && d.contains("new.txt"),
            "and names the untracked file:\n{d}"
        );

        // git's own refusal, not ours: no --force was passed.
        assert!(
            remove_worktree(&repo_path, &wt_path).is_err(),
            "git must refuse to remove a dirty worktree"
        );

        commit_all(&wt_path, "work");
        assert_eq!(unmerged(&repo_path, "main", "agent/task1").unwrap(), 1);

        merge(&repo_path, "main", "agent/task1").expect("merge");
        assert_eq!(unmerged(&repo_path, "main", "agent/task1").unwrap(), 0);
        remove_worktree(&repo_path, &wt_path).expect("clean worktree is removable");
        delete_branch(&repo_path, "agent/task1").expect("merged branch is deletable");
    }

    /// The message a person sees has to name the subcommand that failed, since
    /// git's own text rarely does.
    #[test]
    fn a_failed_git_command_reports_its_subcommand_and_gits_own_words() {
        let d = tempfile::tempdir().unwrap();
        let err = repo_root(d.path().to_str().unwrap()).expect_err("not a repository");
        let text = err.to_string();
        assert!(
            text.starts_with("git rev-parse --show-toplevel: "),
            "message names the command: {text}"
        );
        assert!(
            text.contains("not a git repository"),
            "and keeps git's own words: {text}"
        );
    }

    /// A branch that does not exist makes `rev-list` fail, and that must arrive
    /// as a git failure rather than as a count of zero.
    #[test]
    fn an_unknown_branch_is_a_git_failure_not_a_zero_count() {
        let repo = fixture_repo();
        let p = repo.path().to_str().unwrap();
        assert!(matches!(
            unmerged(p, "main", "agent/absent"),
            Err(Error::Git { .. })
        ));
    }
}
