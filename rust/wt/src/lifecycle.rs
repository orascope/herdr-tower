// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Starting a task, finishing one, and refusing to finish one that would lose
//! work.

use std::io::Write;
use std::path::{Path, PathBuf};

use herdrx::{Herdr, StartOptions};
use time::OffsetDateTime;

use crate::git;
use crate::meta::{delete_meta, load_meta, reserve_meta, Meta};
use crate::{Error, Result};

/// The agent kinds `wt new` will launch: every kind the tower supervises.
///
/// A list of names rather than a map from kind to command: the command equals
/// the kind for all but gemini, whose real command is decided at launch by
/// [`command_for`]. What is left to do here is validating the `--agent` flag,
/// so that is all this is. herdr starts each of them by kind — opencode
/// included — so a kind the tower can supervise is one it can launch.
pub const AGENTS: [&str; tower_state::SUPERVISED_KINDS.len()] = tower_state::SUPERVISED_KINDS;

/// The launch command for an agent kind, always with its auto-approve flags.
///
/// Always: every agent the tower starts runs without approval prompts, with no
/// setting to turn that off (2026-10-02). The `worker_auto_approve` switch that
/// used to sit here let a pane come up asking before each tool, which is the
/// one thing a pane started from the tower must not do.
///
/// `gemini_dir` is a parameter because the answer depends on the filesystem and
/// this is the one part of the decision worth testing without a real home
/// directory.
pub fn command_for(agent: &str, gemini_dir: &Path) -> String {
    let base = if agent == "gemini" {
        gemini_base(gemini_dir)
    } else {
        agent
    };
    with_auto_approve(base)
}

/// The flags that start a CLI without asking before each tool it runs, by the
/// binary a command line starts with.
///
/// Every kind the tower launches, not only gemini. It used to be gemini alone,
/// on the reasoning that claude follows the user's own auto mode and codex its
/// trusted setting — but a worker or an agent `open` started from a phone then
/// sat at its first approval prompt until someone came to the window, which is
/// the thing launching it from the tower was meant to avoid (2026-09-26).
/// Codex gets `-s danger-full-access -a never`, the spelling the user runs it
/// with by hand: no sandbox and no approvals, the same trust the others get.
fn approve_flags(bin: &str) -> Option<&'static str> {
    match bin {
        "claude" => Some("--dangerously-skip-permissions"),
        "codex" => Some("-s danger-full-access -a never"),
        "opencode" => Some("--auto"),
        "agy" => Some("--dangerously-skip-permissions"),
        "gemini" => Some("--yolo"),
        _ => None,
    }
}

/// `command` with its binary's auto-approve flags put right after the binary.
///
/// Right after, not at the end: `codex … resume <id>` and `claude --resume
/// <id>` take the flags as global options before the subcommand or the
/// resume flag, and a trailing `-a never` after an ID would be read as more of
/// the resume arguments. A command whose binary has no known flags, or that
/// already carries them, comes back unchanged — so applying it twice is
/// applying it once.
pub fn with_auto_approve(command: &str) -> String {
    let trimmed = command.trim_start();
    let (bin, rest) = trimmed
        .split_once(char::is_whitespace)
        .map_or((trimmed, ""), |(b, r)| (b, r));
    let Some(flags) = approve_flags(bin) else {
        return command.to_string();
    };
    let already = format!(" {rest} ");
    if already.contains(&format!(" {flags} ")) {
        return command.to_string();
    }
    if rest.is_empty() {
        format!("{bin} {flags}")
    } else {
        format!("{bin} {flags} {rest}")
    }
}

/// `~/.gemini`, the Gemini CLI ecosystem root.
///
/// An empty `PathBuf` when there is no `HOME`, which makes the Antigravity
/// probe below look at a relative path that will not exist — so a machine with
/// no home falls back to the stock CLI rather than guessing.
pub fn gemini_dir() -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => tower_usage::gemini_dir(Path::new(&home)),
        _ => PathBuf::new(),
    }
}

/// `agy` when the Antigravity CLI has left a trace, else the stock CLI.
///
/// Deliberately not a second copy of the rule. Which binary starts a
/// `gemini`-kind agent is decided once, in [`tower_usage::gemini_command`], because
/// the tower reads that agent's context usage from whichever CLI actually ran.
/// If these two ever disagreed, the launcher would start `agy` while the usage
/// reader looked for stock `gemini` logs, and the symptom would be an agent
/// that works fine and simply reports no context — the kind of divergence
/// nobody traces back to a duplicated four-line predicate.
fn gemini_base(gemini_dir: &Path) -> &'static str {
    tower_usage::gemini_command(gemini_dir)
}

/// What `wt new` needs to know.
#[derive(Debug, Default)]
pub struct NewOptions<'a> {
    pub task: &'a str,
    /// Any path inside the target repository — normalized by
    /// [`crate::repo_root`].
    pub repo: &'a str,
    /// Empty means the branch HEAD is currently on.
    pub base: &'a str,
    /// Empty means `claude`.
    pub agent: &'a str,
    pub test_cmd: &'a str,
    /// The backend to make the window in, or `None` to create no window at all.
    ///
    /// One field rather than a "no window" flag beside a backend handle, so "want
    /// a window, have no backend" is not a state that can be built.
    ///
    /// There is no `AcceptPrompts` callback here either. herdr starts agents
    /// itself, so nothing has to type the command and watch the screen. See
    /// `open_window` below.
    pub window: Option<&'a Herdr>,
    /// The pane asking for the task, kept on the record as its coordinator.
    pub parent: Option<crate::Parent>,
}

/// Starts one task: record, then worktree and branch, then a window, then the
/// agent.
///
/// The order is not incidental. The record goes first so a failure anywhere
/// after it leaves evidence; a failed `git worktree add` takes the record back
/// down with it, because there is nothing to point at. A failed *launch* keeps
/// both — the worktree is usable by hand and shows up in `wt list`, so throwing
/// it away would cost more than it saves.
///
/// `now` is a parameter, as in every other crate here: the clock is read once at
/// the entry point and threaded down, so nothing below has a hidden dependency
/// on the wall clock.
pub fn new_task(state_dir: &Path, o: &NewOptions, now: OffsetDateTime) -> Result<Meta> {
    if o.task.is_empty() {
        return Err(Error::NoTask);
    }
    let agent = if o.agent.is_empty() {
        "claude"
    } else {
        o.agent
    };
    if !AGENTS.contains(&agent) {
        return Err(Error::UnknownAgent(agent.to_string()));
    }
    let repo = git::repo_root(o.repo).map_err(|_| Error::NotARepo(o.repo.to_string()))?;
    let base = if o.base.is_empty() {
        match git::current_branch(&repo) {
            Ok(b) if !b.is_empty() => b,
            // Detached HEAD, or git could not answer. Either way there is no
            // base to cut from and guessing one would branch off the wrong
            // commit silently.
            _ => return Err(Error::NoBase),
        }
    } else {
        o.base.to_string()
    };
    let branch = format!("agent/{}", o.task);
    if git::branch_exists(&repo, &branch) {
        return Err(Error::BranchExists(branch));
    }
    // Note this is `Path::join`, which does not normalize: a task named
    // `a/../b` yields a literal `.../a/../b`, not `.../b`. Both resolve to the
    // same directory, so git and every later `-C` agree; only the recorded
    // string differs.
    let path = Path::new(&repo)
        .join(".herdr-tower")
        .join("worktrees")
        .join(o.task)
        .to_string_lossy()
        .into_owned();

    let m = Meta {
        task: o.task.to_string(),
        repo: repo.clone(),
        base: base.clone(),
        branch: branch.clone(),
        path: path.clone(),
        agent: agent.to_string(),
        test_cmd: o.test_cmd.to_string(),
        test_pass: None,
        test_at: None,
        created_at: now,
        drive: None,
        parent: o.parent.clone(),
    };
    // Before the record and before git: a name herdr is already using cannot be
    // launched, and finding that out after the worktree exists is the expensive
    // way to learn it.
    if let Some(h) = o.window {
        let name = agent_name(o.task);
        if name_is_taken(h, &name) {
            return Err(Error::AgentNameTaken(name));
        }
    }
    // The record precedes any filesystem work, and claiming it *is* the check
    // that the task is free — one step, not a look followed by a write. The
    // rollback below is then safe by construction: it can only delete a record
    // this call created, where before it could delete a concurrent winner's.
    reserve_meta(state_dir, &m)?;
    // Before the worktree exists, so the main checkout never reports it. A
    // failure here costs `git status` noise, not the task.
    let _ = git::ignore_generated_worktrees(&repo);
    if let Err(e) = git::add_worktree(&repo, &base, &branch, &path) {
        // Only when nothing survived. `git worktree add -b` creates the branch
        // and *then* checks out, so a checkout that fails — a smudge filter that
        // refuses, a path that cannot be written — exits nonzero with the branch
        // already there. Dropping the record then left an orphan: the next
        // `wt new` was refused by the branch that exists, and `wt list` could not
        // explain what was refusing it because the record was gone.
        if add_left_nothing(&repo, &branch, &path) {
            let _ = delete_meta(state_dir, o.task);
        }
        return Err(e);
    }
    if let Some(h) = o.window {
        let command = command_for(agent, &gemini_dir());
        if let Err(e) = open_window(h, o.task, &path, &command, agent) {
            return Err(Error::WorkerStart {
                meta: Box::new(m),
                source: Box::new(e),
            });
        }
    }
    Ok(m)
}

/// Whether a failed `git worktree add` left nothing behind.
///
/// Both halves matter. `-b` creates the branch first, so the branch can outlive a
/// failed checkout; and a checkout that got part-way leaves a directory. Either
/// one means the record still describes something real, and `wt list` and
/// `wt clean` are the only things that can name it.
fn add_left_nothing(repo: &str, branch: &str, path: &str) -> bool {
    !git::branch_exists(repo, branch) && !Path::new(path).exists()
}

/// Makes the task's window and has herdr launch the agent in it. Returns the
/// pane ID.
///
/// The launch is handed to the backend rather than typed. herdr watches its own
/// integration come up, answers once the agent is ready for input, and reports a
/// startup question as *blocked* instead of leaving it to be guessed at. The
/// path this replaced created a shell pane, typed the command, then read the
/// screen looking for "do you trust the files in this folder?" and pressed
/// Enter — which broke the moment Claude Code made that dialog's default button
/// "No, exit", because the Enter then pressed *decline* (measured 2026-09-11).
/// Screen-scraping a startup handshake is only ever as correct as the last CLI
/// release.
///
/// The pane is created empty and the agent started into it because that is what
/// `agent.start` requires: it never creates, splits, or moves layout.
fn open_window(h: &Herdr, task: &str, path: &str, command: &str, kind: &str) -> Result<String> {
    if !herdrx::inside() {
        return Err(Error::OutsideHerdr);
    }
    let pane_id = tower_mux::spawn_in_focused(h, task, path, None)?;
    if let Err(source) = start_in(h, &pane_id, task, command, kind) {
        // The pane stays. Somebody has to be able to attach and see what
        // blocked, and a pane nobody can name is worse than a stray one.
        return Err(Error::AgentStart { pane_id, source });
    }
    Ok(pane_id)
}

/// Has herdr launch the task's agent in `pane_id`, a pane just made for it.
///
/// A new tab's shell reaches its prompt a moment after the tab: `agent.start`
/// 13–63ms after the shell was spawned was refused with "agent target pane
/// w7:pBY is not an available shell", and one 111ms after went in (herdr
/// server log, load 40, measured 2026-10-24). So that refusal is waited out.
fn start_in(h: &Herdr, pane_id: &str, task: &str, command: &str, kind: &str) -> herdrx::Result<()> {
    let (bin, args) = split_command(command);
    let opts = StartOptions {
        name: agent_name(task),
        kind: agent_kind(kind, bin).to_string(),
        pane_id: pane_id.to_string(),
        args: args.into_iter().map(str::to_string).collect(),
        // herdr's own 30s readiness default.
        timeout: None,
    };
    herdrx::retry_while("agent_pane_busy", || h.start_agent(&opts))
}

/// Splits a launch command into its executable and its arguments.
///
/// Whitespace is the only separator and quoting is not a thing here, which is
/// safe because the only strings that arrive are the ones [`command_for`] builds.
/// Note what the executable is used for: **not** running anything — herdr
/// launches the binary for the kind — only for [`agent_kind`] to notice that the
/// binary is `agy`.
pub fn split_command(command: &str) -> (&str, Vec<&str>) {
    let mut fields = command.split_whitespace();
    match fields.next() {
        Some(bin) => (bin, fields.collect()),
        None => ("", Vec::new()),
    }
}

/// The kind name the backend knows this agent by.
///
/// herdr-tower folds agy into gemini for supervision; herdr recognizes them
/// separately. When there is a real binary name it is the more precise answer.
pub fn agent_kind<'a>(kind: &'a str, bin: &str) -> &'a str {
    if bin == "agy" {
        "agy"
    } else {
        kind
    }
}

/// A task name bent into herdr's agent-name rule, `[a-z][a-z0-9_-]{0,31}`.
///
/// herdr rejects the launch outright on a name that does not match, so this is
/// not cosmetic — a task called `2단계` cannot be handed over as it is.
///
/// Lowercasing is Rust's `to_lowercase`, which is *full* case mapping: a
/// character whose lowercase expands keeps the expansion — `İ` (U+0130) becomes
/// `i` + a combining dot, which the filter turns into `i-`. That is harmless:
/// these are sanitized names for a launch, nothing on disk keys off them, and no
/// character that survives the filter is affected.
pub fn agent_name(task: &str) -> String {
    let mapped: String = task
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            _ => '-',
        })
        .collect();
    let mut name = mapped.trim_matches(|c| c == '-' || c == '_').to_string();
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        // Covers the empty case too: a name of nothing but separators, or one
        // starting with a digit, is not a legal herdr name.
        name.insert(0, 'w');
    }
    // A suffix derived from the *original* task, because the normalization above
    // is lossy: `foo.bar` and `foo-bar` both reduce to `foo-bar`, as does
    // anything sharing its first 32 normalized characters. herdr requires the
    // agent name to be unique, so the second `wt new` failed — after creating
    // its worktree and an empty tab, which is the worst moment to fail.
    //
    // Derived rather than random so relaunching one task twice yields one name;
    // two tasks cannot collide here because `wt new` already refuses a duplicate
    // task name.
    // Thirty-two bits, not sixteen. Two task names that normalize together *and*
    // share a 16-bit digest are findable by hand —
    // `abcdefghijklmnopqrstuvwxyzx415` and `…x527` both became
    // `abcdefghijklmnopqrstuvwxyzx-5a72` — and the second `wt new` then created
    // its branch, worktree and tab before herdr refused the duplicate name.
    let suffix = format!("-{:08x}", name_hash(task));
    // Every surviving character is ASCII, so truncating bytes is truncating
    // characters and cannot split one.
    name.truncate(32 - suffix.len());
    name.push_str(&suffix);
    name
}

/// A 32-bit digest of the task name, for distinguishing names that normalize
/// together.
///
/// FNV-1a, written out rather than pulled in: this is four lines, needs no
/// collision resistance against an adversary, and the alternative is a
/// dependency in a crate that has none. Folded to 32 bits rather than 16 because
/// the suffix is what keeps two normalized names apart, and eight hex digits is
/// still eight characters out of thirty-two.
fn name_hash(s: &str) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h ^ (h >> 32)) as u32
}

/// Refuses a task whose agent name herdr is already using.
///
/// A digest makes a collision improbable, not impossible, and the cost of one
/// falls at the worst moment: `start_agent` refuses *after* the branch, the
/// worktree and the tab exist. Asked here, before anything is created, it costs
/// one `pane.list` and the answer is a sentence instead of debris.
///
/// A backend that cannot be listed is not evidence of a collision — the launch
/// itself will report whatever is really wrong.
///
/// This read the pane's name through a field that never deserialized (the wire
/// key is `label`), so it answered `false` for every pane and the guard has
/// never once fired. Every collision it was built to catch was instead met by
/// `start_agent` refusing after the branch, the worktree and the tab already
/// existed — the debris this function exists to prevent.
fn name_is_taken(h: &Herdr, name: &str) -> bool {
    h.list_panes()
        .map(|panes| any_pane_named(&panes, name))
        .unwrap_or(false)
}

/// The predicate on its own, so it can be tested without a server.
///
/// Splitting it out is the point: while this lived inside the call that fetches
/// the panes, the only way to exercise it was against a live herdr, so nothing
/// did — and the field it reads could be wrong for as long as it was.
///
/// Compared under the `[w2:p1] ` prefix the tower names its panes with, so a
/// pane it named `[w2:p1] auth-api-3f2a91cc` still takes that name.
fn any_pane_named(panes: &[herdrx::Pane], name: &str) -> bool {
    panes.iter().any(|p| {
        p.label
            .as_deref()
            .is_some_and(|n| herdrx::strip_id_prefix(n) == name)
    })
}

/// Preservation-first cleanup: one uncommitted file or one unmerged commit and
/// nothing is deleted.
///
/// The refusal is a value ([`Error::CleanRefused`]) rather than a message so the
/// caller can tell it from a real failure. Both checks run before either
/// deletion, and both swallow their own errors — an unreadable worktree reports
/// nothing dirty rather than aborting.
///
/// Unmerged commits are counted on the branch `wt new` cut *and* on the one
/// the worktree is on now, when a worker moved to a branch of its own: work
/// committed there is still work, and removing its worktree on the strength
/// of the recorded branch being empty would take it off every screen.
pub fn clean(state_dir: &Path, task: &str) -> Result<()> {
    let m = load_meta(state_dir, task)?;
    let dirty = git::dirty(&m.path).unwrap_or_default();
    let one = |branch: &str| git::unmerged(&m.repo, &m.base, branch).unwrap_or(0);
    let unmerged = match git::checked_out_branch(&m.path).filter(|b| *b != m.branch) {
        // Once each, a commit on both counted once; when one branch cannot be
        // read (deleted), each alone, so it still refuses on the other's work.
        Some(live) => git::unmerged_either(&m.repo, &m.base, &m.branch, &live)
            .unwrap_or_else(|_| one(&m.branch) + one(&live)),
        None => one(&m.branch),
    };
    if !dirty.is_empty() || unmerged > 0 {
        return Err(Error::CleanRefused { dirty, unmerged });
    }
    // Each step skipped when it has already happened, so a `clean` that failed
    // half-way can be run again. `git branch -d` refuses a branch that is not
    // merged into the *current* HEAD — these branches are cut from `base` and the
    // main checkout is often on something else — and the first attempt had already
    // removed the worktree by then. Every retry then failed at the absent
    // worktree, before reaching the branch, and the task sat in `wt list` for
    // good.
    if Path::new(&m.path).exists() {
        git::remove_worktree(&m.repo, &m.path)?;
    }
    if git::branch_exists(&m.repo, &m.branch) {
        git::delete_branch(&m.repo, &m.branch)?;
    }
    delete_meta(state_dir, task)
}

/// Shows what a merge would do and the exact commands it will run, then runs
/// them only if `confirm` says yes.
///
/// There is no automatic merge anywhere in this crate, on purpose: the commands
/// are printed in full so that saying no still leaves the user able to do it by
/// hand.
pub fn merge_guide<W: Write>(
    w: &mut W,
    state_dir: &Path,
    task: &str,
    confirm: impl FnOnce() -> bool,
) -> Result<()> {
    let m = load_meta(state_dir, task)?;
    // 🚨 A worktree moved off the branch `wt new` cut has its work on the
    // other branch. Merging the recorded one would merge the wrong tree — or,
    // with nothing on it, say "nothing to merge" about unmerged work. Which of
    // the two branches should land is the person's call, so this names both
    // and merges neither.
    if let Some(live) = git::checked_out_branch(&m.path).filter(|b| *b != m.branch) {
        writeln!(w, "task {} — {} ← {}", m.task, m.base, m.branch)?;
        writeln!(
            w,
            "  ⚠ the worktree is on {live}, not {} — its work is there, so this merges nothing.",
            m.branch
        )?;
        writeln!(w, "  to merge that branch yourself:")?;
        writeln!(w, "  git -C {} checkout {}", m.repo, m.base)?;
        writeln!(w, "  git -C {} merge --no-ff {live}", m.repo)?;
        return Ok(());
    }
    let dirty = git::dirty(&m.path).unwrap_or_default();
    let unmerged = git::unmerged(&m.repo, &m.base, &m.branch).unwrap_or(0);

    writeln!(w, "task {} — {} ← {}", m.task, m.base, m.branch)?;
    // 🚨 A pass is reported against the commit it was measured on. `test_pass`
    // and `test_at` carry no commit, so a run from last night reads exactly
    // like one from a minute ago — and this is the command that actually
    // merges. `Drive::describes` already answers "is that verdict still about
    // this tree"; without consulting it here, the binding protected `wt list`
    // and left the merge itself offering a commit no test ever saw.
    //
    // Asked of HEAD alone, not of `describes`. For a merge-ready verdict
    // `describes` is also false when the tree merely has uncommitted edits —
    // and then "the branch has moved" is untrue: the branch is still the
    // commit that was tested, and the edits are the separate problem the
    // uncommitted-files warning below already names.
    let superseded = m
        .drive
        .as_ref()
        .is_some_and(|d| !d.head.is_empty() && git::head(&m.path).is_ok_and(|now| now != d.head));
    match m.test_pass {
        Some(true) if superseded => writeln!(
            w,
            "  tests: ✔ passed ({}) — but the branch has moved since; run wt test again",
            hhmm(m.test_at)
        )?,
        Some(true) => writeln!(w, "  tests: ✔ passed ({})", hhmm(m.test_at))?,
        Some(false) => writeln!(
            w,
            "  tests: ✖ failed ({}) — look before merging",
            hhmm(m.test_at)
        )?,
        None => writeln!(w, "  tests: not run (wt test runs them)")?,
    }
    if !dirty.is_empty() {
        writeln!(
            w,
            "  ⚠ {} uncommitted in the worktree — the merge leaves them behind:",
            dirty.len()
        )?;
        for d in &dirty {
            // Five spaces, not four, on purpose: it is the indent this line has
            // always printed, and "tidying" it changes the output.
            writeln!(w, "     {d}")?;
        }
    }
    if unmerged == 0 {
        writeln!(w, "  nothing to merge.")?;
        return Ok(());
    }
    writeln!(w, "  commits to merge: {unmerged}\n\ncommands to run:")?;
    writeln!(w, "  git -C {} checkout {}", m.repo, m.base)?;
    writeln!(w, "  git -C {} merge --no-ff {}\n", m.repo, m.branch)?;
    write!(w, "Go ahead? [y/N] ")?;
    if !confirm() {
        writeln!(w, "cancelled.")?;
        return Ok(());
    }
    git::merge(&m.repo, &m.base, &m.branch).map_err(|e| Error::MergeFailed(Box::new(e)))?;
    writeln!(w, "merged. wt clean tidies up.")?;
    Ok(())
}

/// `HH:MM` in the timestamp's own offset, not converted to the local zone.
///
/// A record with `test_pass` set always has `test_at` set — they are written
/// together — so the `None` arm is unreachable through this crate. It prints
/// `--:--` rather than panicking because a hand-edited record must not take the
/// merge guide down.
fn hhmm(t: Option<OffsetDateTime>) -> String {
    match t {
        Some(t) => format!("{:02}:{:02}", t.hour(), t.minute()),
        None => "--:--".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{commit_all, fixture_repo};
    use crate::meta::list_metas;
    use crate::meta::save_meta;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_789_000_496).unwrap()
    }

    fn opts<'a>(task: &'a str, repo: &'a str) -> NewOptions<'a> {
        NewOptions {
            task,
            repo,
            ..NewOptions::default()
        }
    }

    #[test]
    fn a_new_task_creates_a_branch_a_worktree_and_a_record() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();

        let m = new_task(state.path(), &opts("auth-api", repo_path), now()).unwrap();
        assert_eq!(m.base, "main");
        assert_eq!(m.branch, "agent/auth-api");
        assert_eq!(m.agent, "claude", "claude is the default");
        assert!(
            Path::new(&m.path).join("a.txt").is_file(),
            "the worktree carries the repository's files"
        );
        assert!(git::branch_exists(repo_path, "agent/auth-api"));
        assert_eq!(list_metas(state.path()).unwrap().len(), 1);
    }

    #[test]
    fn a_task_name_can_only_be_used_once() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();
        new_task(state.path(), &opts("auth-api", repo_path), now()).unwrap();
        // The branch check fires before the record check, so this is the error
        // about the branch — either way the name is refused.
        assert!(new_task(state.path(), &opts("auth-api", repo_path), now()).is_err());
    }

    /// Two `wt new` for one task: the loser must not take the winner's record
    /// with it.
    ///
    /// Both used to pass the "does this task exist" check, both save the record,
    /// and then one `git worktree add` wins while the other's rollback deletes the
    /// shared record — leaving a real branch and worktree that `wt list`, `wt
    /// merge` and `wt clean` could no longer find. The claim is now the check, so
    /// the loser never reaches the rollback.
    ///
    /// Spelled as the rollback itself rather than as two threads: the damage was
    /// never in the timing, it was in deleting a record this attempt did not own.
    #[test]
    fn a_losing_wt_new_leaves_the_winners_record_alone() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();
        let winner = new_task(state.path(), &opts("t1", repo_path), now()).unwrap();

        // The second attempt: the name is taken, so it stops at the claim.
        assert!(matches!(
            new_task(state.path(), &opts("t1", repo_path), now()),
            Err(Error::TaskExists(_)) | Err(Error::BranchExists(_))
        ));

        let still = load_meta(state.path(), "t1").expect("the winner's record survives");
        assert_eq!(still.path, winner.path);
        assert_eq!(still.branch, winner.branch);
    }

    /// A generated worktree must not show up as untracked in the main checkout.
    ///
    /// `wt new` populates `<repo>/.herdr-tower/worktrees/<task>`, and without a
    /// rule the whole worker tree is untracked in the repository it was cut from
    /// — where a later `git add .` stages it.
    #[test]
    fn a_new_task_leaves_the_main_checkout_clean() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();
        new_task(state.path(), &opts("t1", repo_path), now()).unwrap();

        let untracked = git::dirty(repo_path).unwrap_or_default();
        assert!(
            untracked.is_empty(),
            "the worker tree is showing in the repository it was cut from: {untracked:?}"
        );

        // And the rule went into the local exclude, not into a tracked file.
        assert!(
            !repo.path().join(".gitignore").exists(),
            "a tool must not edit the project's own ignore file"
        );
        let exclude =
            std::fs::read_to_string(repo.path().join(".git").join("info").join("exclude"))
                .unwrap_or_default();
        assert!(exclude.contains("/.herdr-tower/"), "{exclude:?}");

        // Twice is once: the rule is not appended again.
        new_task(state.path(), &opts("t2", repo_path), now()).unwrap();
        let again = std::fs::read_to_string(repo.path().join(".git").join("info").join("exclude"))
            .unwrap_or_default();
        assert_eq!(again.matches("/.herdr-tower/").count(), 1, "{again:?}");
    }

    /// `clean` can be run again after it failed half-way.
    ///
    /// `git branch -d` refuses a branch that is not merged into the current HEAD,
    /// and by then the worktree is already gone — so the retry used to fail at the
    /// absent worktree and the task stayed in `wt list` for good.
    #[test]
    fn clean_resumes_after_the_branch_deletion_failed() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(state.path(), &opts("t1", repo_path), now()).unwrap();

        // The state the first attempt leaves behind when `-d` refuses.
        git::remove_worktree(&m.repo, &m.path).unwrap();
        assert!(!Path::new(&m.path).exists());
        assert!(git::branch_exists(&m.repo, &m.branch));

        clean(state.path(), "t1").expect("the retry has to get past the absent worktree");
        assert!(!git::branch_exists(&m.repo, &m.branch), "branch removed");
        assert!(load_meta(state.path(), "t1").is_err(), "record removed");
    }

    /// A failed `git worktree add` that left artifacts keeps its record.
    ///
    /// `-b` creates the branch before the checkout, so a checkout that fails — a
    /// smudge filter that refuses, a path that cannot be written — exits nonzero
    /// with the branch already there. Deleting the record then left an orphan
    /// nothing could name: the next `wt new` was refused by the branch, and
    /// `wt list` had nothing to show that explained it.
    ///
    /// The rule is tested rather than the scenario: making `add` fail *after* it
    /// creates the branch needs a repository rigged with a failing filter, and
    /// what has to be right is the question asked afterwards.
    #[test]
    fn a_rollback_only_removes_a_record_with_nothing_behind_it() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(state.path(), &opts("t1", repo_path), now()).unwrap();

        // Branch and worktree both present: nothing to roll back.
        assert!(!add_left_nothing(&m.repo, &m.branch, &m.path));

        // The worktree gone, the branch still there — the half-failed shape.
        git::remove_worktree(&m.repo, &m.path).unwrap();
        assert!(!Path::new(&m.path).exists());
        assert!(git::branch_exists(&m.repo, &m.branch));
        assert!(
            !add_left_nothing(&m.repo, &m.branch, &m.path),
            "a surviving branch is an artifact the record has to keep naming"
        );

        // Neither: the record describes nothing and may go.
        git::delete_branch(&m.repo, &m.branch).unwrap();
        assert!(add_left_nothing(&m.repo, &m.branch, &m.path));
    }

    /// The reservation holds its lock across the read *and* the write, so no
    /// concurrent caller can look into the moment between them.
    ///
    /// `create_new` publishes an empty path first and fills it a moment later. A
    /// second `wt new` looking then found a file it could not parse, called the
    /// record corrupt, and took it over — and both callers went on to create a
    /// worktree while only one record survived.
    #[test]
    fn a_reservation_is_not_visible_until_it_is_complete() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(state.path(), &opts("t1", repo_path), now()).unwrap();

        // Whoever holds the directory lock keeps the next reservation out.
        let held = crate::meta::claim_lock(&crate::meta::dir(state.path()))
            .expect("the lock file has to be creatable");
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (dir, flag) = (state.path().to_path_buf(), std::sync::Arc::clone(&done));
        let mut second = m.clone();
        second.branch = "agent/t2".into();
        let t = std::thread::spawn(move || {
            let refused = reserve_meta(&dir, &second).is_err();
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            refused
        });

        std::thread::sleep(std::time::Duration::from_millis(120));
        assert!(
            !done.load(std::sync::atomic::Ordering::SeqCst),
            "a reservation read the directory while another held it"
        );
        drop(held);
        assert!(t.join().unwrap(), "the name was already taken");
        assert_eq!(load_meta(state.path(), "t1").unwrap().branch, m.branch);
    }

    /// A corrupt record must not make a name unusable — `load_meta` documents
    /// that, and the exclusive claim must not quietly take it away.
    #[test]
    fn a_corrupt_record_is_taken_over_rather_than_refused() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(crate::meta::dir(state.path())).unwrap();
        std::fs::write(
            crate::meta::dir(state.path()).join("t1.json"),
            b"{ this is not json",
        )
        .unwrap();

        let m = new_task(state.path(), &opts("t1", repo_path), now())
            .expect("a record nobody can read is not a task");
        assert_eq!(m.task, "t1");
        assert_eq!(load_meta(state.path(), "t1").unwrap().branch, m.branch);
    }

    /// A record with no branch behind it (debris, or a hand-deleted branch) is
    /// still enough to refuse the name, which is what keeps `wt list` honest.
    #[test]
    fn an_existing_record_alone_refuses_the_name() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(state.path(), &opts("t1", repo_path), now()).unwrap();
        git::remove_worktree(&m.repo, &m.path).unwrap();
        git::delete_branch(&m.repo, &m.branch).unwrap();
        assert!(matches!(
            new_task(state.path(), &opts("t1", repo_path), now()),
            Err(Error::TaskExists(_))
        ));
    }

    #[test]
    fn bad_input_is_refused_before_anything_is_created() {
        let state = tempfile::tempdir().unwrap();
        let not_a_repo = tempfile::tempdir().unwrap();
        let not_a_repo = not_a_repo.path().to_str().unwrap();

        assert!(matches!(
            new_task(state.path(), &opts("", not_a_repo), now()),
            Err(Error::NoTask)
        ));
        assert!(matches!(
            new_task(state.path(), &opts("x", not_a_repo), now()),
            Err(Error::NotARepo(_))
        ));

        let repo = fixture_repo();
        assert!(matches!(
            new_task(
                state.path(),
                &NewOptions {
                    task: "x",
                    repo: repo.path().to_str().unwrap(),
                    agent: "gpt9",
                    ..NewOptions::default()
                },
                now()
            ),
            Err(Error::UnknownAgent(a)) if a == "gpt9"
        ));
        assert!(
            list_metas(state.path()).unwrap().is_empty(),
            "a refused task leaves no record"
        );
    }

    /// A worktree git will not create must not leave a record pointing at
    /// nothing — the record is written first precisely so this rollback exists.
    #[test]
    fn a_failed_worktree_takes_its_record_back_down() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();
        // A base that does not exist: the record is saved, then git refuses.
        let err = new_task(
            state.path(),
            &NewOptions {
                task: "t1",
                repo: repo_path,
                base: "no-such-base",
                ..NewOptions::default()
            },
            now(),
        )
        .expect_err("git must refuse an unknown base");
        assert!(matches!(err, Error::Git { .. }));
        assert!(list_metas(state.path()).unwrap().is_empty());
    }

    /// The pane that asked is kept on the record as the task's coordinator,
    /// and reads back from disk; a task nobody's pane asked for has none.
    #[test]
    fn the_asking_pane_is_kept_as_the_coordinator() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let parent = crate::Parent {
            backend: String::new(),
            pane_id: "w4F:pC".into(),
        };
        let m = new_task(
            state.path(),
            &NewOptions {
                parent: Some(parent.clone()),
                ..opts("t1", repo.path().to_str().unwrap())
            },
            now(),
        )
        .unwrap();
        assert_eq!(m.parent, Some(parent.clone()));
        assert_eq!(
            crate::load_meta(state.path(), "t1").unwrap().parent,
            Some(parent)
        );
        let m = new_task(
            state.path(),
            &opts("t2", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        assert_eq!(m.parent, None);
    }

    #[test]
    fn cleanup_refuses_uncommitted_work_and_deletes_nothing() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t1", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();

        std::fs::write(Path::new(&m.path).join("wip.txt"), "wip\n").unwrap();
        match clean(state.path(), "t1") {
            Err(Error::CleanRefused { dirty, unmerged }) => {
                assert_eq!(dirty.len(), 1);
                assert_eq!(unmerged, 0);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(
            Path::new(&m.path).is_dir(),
            "a refusal preserves the worktree"
        );
    }

    #[test]
    fn cleanup_refuses_unmerged_commits_and_deletes_nothing() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t1", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        std::fs::write(Path::new(&m.path).join("wip.txt"), "wip\n").unwrap();
        commit_all(&m.path, "wip");

        match clean(state.path(), "t1") {
            Err(Error::CleanRefused { dirty, unmerged }) => {
                assert!(dirty.is_empty());
                assert_eq!(unmerged, 1);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(Path::new(&m.path).is_dir());
    }

    #[test]
    fn cleanup_removes_the_worktree_branch_and_record_once_the_work_has_landed() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t1", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        std::fs::write(Path::new(&m.path).join("wip.txt"), "wip\n").unwrap();
        commit_all(&m.path, "wip");
        git::merge(&m.repo, &m.base, &m.branch).unwrap();

        clean(state.path(), "t1").expect("clean and merged is removable");
        assert!(!Path::new(&m.path).exists());
        assert!(!git::branch_exists(&m.repo, &m.branch));
        assert!(load_meta(state.path(), "t1").is_err());
    }

    /// A worker that moved its worktree to a branch of its own has its commits
    /// there; the recorded branch being empty must not let `clean` remove it.
    #[test]
    fn cleanup_counts_the_branch_the_worktree_moved_to() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t1", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        crate::git::tests::switch_new(&m.path, "agent/explain-2969-stage2");
        std::fs::write(Path::new(&m.path).join("wip.txt"), "wip\n").unwrap();
        commit_all(&m.path, "wip");

        match clean(state.path(), "t1") {
            Err(Error::CleanRefused { dirty, unmerged }) => {
                assert!(dirty.is_empty());
                assert_eq!(unmerged, 1);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(Path::new(&m.path).is_dir());
        assert_eq!(m.live_branch(), "agent/explain-2969-stage2");
    }

    /// Commits made on the recorded branch before the worker branched off it
    /// are on both branches: counted once, not twice.
    #[test]
    fn cleanup_counts_a_commit_on_both_branches_once() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t1", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        for i in 0..2 {
            std::fs::write(Path::new(&m.path).join(format!("a{i}.txt")), "a\n").unwrap();
            commit_all(&m.path, "on the task branch");
        }
        crate::git::tests::switch_new(&m.path, "agent/x");
        std::fs::write(Path::new(&m.path).join("b.txt"), "b\n").unwrap();
        commit_all(&m.path, "on the branch it moved to");

        match clean(state.path(), "t1") {
            Err(Error::CleanRefused { unmerged, .. }) => assert_eq!(unmerged, 3, "2 shared + 1"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A moved worktree whose work has all landed is cleaned like any other:
    /// the worktree and the branch `wt new` cut go, and the branch the worker
    /// moved to — somebody else's name, never the tower's to delete — stays.
    #[test]
    fn cleaning_a_landed_moved_worktree_keeps_the_branch_it_moved_to() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t1", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        crate::git::tests::switch_new(&m.path, "agent/explain-2969-stage2");
        std::fs::write(Path::new(&m.path).join("wip.txt"), "wip\n").unwrap();
        commit_all(&m.path, "wip");
        git::merge(&m.repo, &m.base, "agent/explain-2969-stage2").unwrap();

        clean(state.path(), "t1").expect("everything landed");
        assert!(!Path::new(&m.path).exists());
        assert!(
            !git::branch_exists(&m.repo, &m.branch),
            "the cut branch goes"
        );
        assert!(
            git::branch_exists(&m.repo, "agent/explain-2969-stage2"),
            "the branch it moved to stays"
        );
    }

    /// A detached HEAD is no branch to have moved to: `merge` goes by the
    /// recorded branch as before, and says there is nothing on it.
    #[test]
    fn a_detached_worktree_is_not_read_as_moved() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t1", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        crate::git::tests::detach(&m.path);
        let mut buf = Vec::new();
        merge_guide(&mut buf, state.path(), "t1", || {
            panic!("nothing to confirm")
        })
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(!out.contains("the worktree is on"), "{out}");
        assert!(out.contains("nothing to merge."), "{out}");
    }

    /// Merging the recorded branch of a moved worktree merges the wrong tree:
    /// both branches are named and nothing is merged, even on a yes.
    #[test]
    fn a_moved_worktree_is_named_and_not_merged() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t1", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        crate::git::tests::switch_new(&m.path, "agent/explain-2969-stage2");
        std::fs::write(Path::new(&m.path).join("wip.txt"), "wip\n").unwrap();
        commit_all(&m.path, "wip");

        let mut buf = Vec::new();
        merge_guide(&mut buf, state.path(), "t1", || {
            panic!("nothing to confirm")
        })
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("the worktree is on agent/explain-2969-stage2, not agent/t1"),
            "{out}"
        );
        assert!(
            out.contains("merge --no-ff agent/explain-2969-stage2"),
            "{out}"
        );
        assert_eq!(
            git::unmerged(&m.repo, &m.base, "agent/explain-2969-stage2").unwrap(),
            1,
            "nothing merged"
        );
    }

    /// The refusal has to say what it found, because "정리 거부" alone does not
    /// tell a person whether to commit or to merge.
    #[test]
    fn a_refusal_names_both_kinds_of_unfinished_work() {
        let e = Error::CleanRefused {
            dirty: vec!["?? a".into(), " M b".into()],
            unmerged: 3,
        };
        assert_eq!(
            e.to_string(),
            "refusing to clean up: 2 uncommitted or untracked, 3 unmerged commits — commit and merge, then try again"
        );
    }

    #[test]
    fn declining_the_merge_guide_leaves_the_branch_unmerged() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(state.path(), &opts("t2", repo_path), now()).unwrap();
        std::fs::write(Path::new(&m.path).join("f.txt"), "x\n").unwrap();
        commit_all(&m.path, "work");

        let mut buf = Vec::new();
        merge_guide(&mut buf, state.path(), "t2", || false).unwrap();
        assert_eq!(git::unmerged(repo_path, "main", "agent/t2").unwrap(), 1);

        let out = String::from_utf8(buf).unwrap();
        assert_eq!(
            out,
            format!(
                "task t2 — main ← agent/t2\n\
                 \x20 tests: not run (wt test runs them)\n\
                 \x20 commits to merge: 1\n\
                 \n\
                 commands to run:\n\
                 \x20 git -C {repo} checkout main\n\
                 \x20 git -C {repo} merge --no-ff agent/t2\n\
                 \n\
                 Go ahead? [y/N] cancelled.\n",
                repo = m.repo
            ),
            "the guide has to print commands a person can run by hand"
        );
    }

    #[test]
    fn accepting_the_merge_guide_merges() {
        let repo = fixture_repo();
        let repo_path = repo.path().to_str().unwrap();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(state.path(), &opts("t2", repo_path), now()).unwrap();
        std::fs::write(Path::new(&m.path).join("f.txt"), "x\n").unwrap();
        commit_all(&m.path, "work");

        let mut buf = Vec::new();
        merge_guide(&mut buf, state.path(), "t2", || true).unwrap();
        assert_eq!(git::unmerged(repo_path, "main", "agent/t2").unwrap(), 0);
        assert!(String::from_utf8(buf).unwrap().contains("merged"));
    }

    /// Uncommitted files are named in the guide because they are exactly what
    /// the merge will *not* carry, and a person about to merge needs to know.
    #[test]
    fn the_merge_guide_warns_about_work_the_merge_would_leave_behind() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t3", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        std::fs::write(Path::new(&m.path).join("wip.txt"), "x\n").unwrap();

        let mut buf = Vec::new();
        merge_guide(&mut buf, state.path(), "t3", || {
            panic!("nothing to merge, so confirmation must never be asked")
        })
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("⚠ 1 uncommitted in the worktree"), "{out}");
        assert!(out.contains("\n     ?? wip.txt\n"), "{out}");
        assert!(out.contains("nothing to merge."), "{out}");
    }

    #[test]
    fn a_recorded_test_result_shows_in_the_merge_guide_with_its_local_time() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t4", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        let at = now().to_offset(time::UtcOffset::from_hms(9, 0, 0).unwrap());
        for (pass, want) in [(true, "✔ passed"), (false, "✖ failed")] {
            let mut stored = m.clone();
            stored.test_pass = Some(pass);
            stored.test_at = Some(at);
            save_meta(state.path(), &stored).unwrap();
            let mut buf = Vec::new();
            merge_guide(&mut buf, state.path(), "t4", || false).unwrap();
            let out = String::from_utf8(buf).unwrap();
            assert!(out.contains(want), "{out}");
            assert!(
                out.contains(&format!("({:02}:{:02})", at.hour(), at.minute())),
                "{out}"
            );
        }
    }

    /// herdr starts opencode by kind like the others, so `wt new` takes it: a
    /// supervised kind that could not be launched as a worker was the second
    /// half of #74's follow-ups.
    #[test]
    fn an_opencode_worker_is_accepted_and_launched_as_opencode() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &NewOptions {
                agent: "opencode",
                ..opts("oc-task", repo.path().to_str().unwrap())
            },
            now(),
        )
        .unwrap();
        assert_eq!(m.agent, "opencode");

        let no_home = tempfile::tempdir().unwrap();
        assert_eq!(command_for("opencode", no_home.path()), "opencode --auto");
        assert_eq!(agent_kind("opencode", "opencode"), "opencode");
    }

    /// Every kind starts without approval prompts, always — each with its own
    /// CLI's spelling. There is no setting that launches one bare.
    #[test]
    fn auto_approve_gives_every_kind_its_own_flags() {
        let no_home = tempfile::tempdir().unwrap();
        for (kind, want) in [
            ("claude", "claude --dangerously-skip-permissions"),
            ("codex", "codex -s danger-full-access -a never"),
            ("opencode", "opencode --auto"),
            ("gemini", "gemini --yolo"),
        ] {
            assert_eq!(command_for(kind, no_home.path()), want, "{kind}");
        }
    }

    /// The flags go right after the binary, so a resume command keeps its
    /// subcommand and ID last; unknown binaries and already-flagged commands
    /// are left alone, which makes the function idempotent.
    #[test]
    fn auto_approve_goes_after_the_binary_and_only_once() {
        let cases = [
            (
                "claude --resume 'abc'",
                "claude --dangerously-skip-permissions --resume 'abc'",
            ),
            (
                "codex resume 'abc'",
                "codex -s danger-full-access -a never resume 'abc'",
            ),
            (
                "opencode --session 'ses_1'",
                "opencode --auto --session 'ses_1'",
            ),
            (
                "agy --conversation 'c1'",
                "agy --dangerously-skip-permissions --conversation 'c1'",
            ),
            ("codex", "codex -s danger-full-access -a never"),
            ("lazygit", "lazygit"),
            ("", ""),
        ];
        for (input, want) in cases {
            let once = with_auto_approve(input);
            assert_eq!(once, want, "{input:?}");
            assert_eq!(with_auto_approve(&once), once, "idempotent for {input:?}");
        }
    }

    /// Which gemini binary depends on whether Antigravity has been installed,
    /// and that decision has to match `tower_usage::gemini_command` exactly — a
    /// worker launched under the wrong one is refused by the vendor, not by us.
    #[test]
    fn gemini_follows_the_antigravity_trace() {
        let home = tempfile::tempdir().unwrap();
        let gemini = home.path().join(".gemini");
        std::fs::create_dir_all(&gemini).unwrap();
        assert_eq!(command_for("gemini", &gemini), "gemini --yolo");

        std::fs::create_dir_all(gemini.join("antigravity-cli")).unwrap();
        assert_eq!(
            command_for("gemini", &gemini),
            "agy --dangerously-skip-permissions"
        );
    }

    /// herdr's rule is `[a-z][a-z0-9_-]{0,31}` and it refuses the launch
    /// outright on anything else, so every one of these is a launch that would
    /// not happen if the name went through unchanged.
    #[test]
    fn an_agent_name_always_satisfies_herdrs_rule() {
        let cases = [
            ("probe", "probe-"),
            ("Hero-Bold", "hero-bold-"),
            ("feat/login form", "feat-login-form-"),
            // Korean becomes separators, and after trimming the name starts
            // with a digit, so it needs the prefix.
            ("2단계", "w2-"),
            ("", "w-"),
            ("---", "w-"),
            (
                "aVeryLongTaskNameThatGoesOnAndOnForever",
                "averylongtasknamethatgo-",
            ),
        ];
        // Two long names that normalize together and used to share a 16-bit
        // digest. The second `wt new` created its branch, worktree and tab before
        // herdr refused the duplicate name.
        assert_ne!(
            agent_name("abcdefghijklmnopqrstuvwxyzx415"),
            agent_name("abcdefghijklmnopqrstuvwxyzx527"),
        );

        for (input, want) in cases {
            let got = agent_name(input);
            assert!(
                got.starts_with(want),
                "agent_name({input:?}) = {got:?}, expected to start with {want:?}"
            );
            assert!(got.len() <= 32, "{got:?} is longer than herdr allows");
            assert!(
                got.starts_with(|c: char| c.is_ascii_lowercase()),
                "{got:?} does not start with a lowercase letter"
            );
            assert!(
                got.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
                "{got:?} carries a character herdr rejects"
            );
        }
    }

    /// Two task names that normalize together must not ask herdr for one name.
    ///
    /// herdr requires the agent name to be unique, so the second `wt new` was
    /// refused — after it had already created its worktree and an empty tab,
    /// which is the worst moment to fail. The normalization is lossy in two
    /// ways: separators all become `-`, and anything past 32 characters is cut.
    #[test]
    fn names_that_normalize_together_stay_distinct() {
        let collide = [
            ("foo.bar", "foo-bar"),
            ("feat/login", "feat login"),
            (
                "aVeryLongTaskNameThatGoesOnAndOnForeverAndEverOne",
                "aVeryLongTaskNameThatGoesOnAndOnForeverAndEverTwo",
            ),
        ];
        for (a, b) in collide {
            assert_ne!(
                agent_name(a),
                agent_name(b),
                "{a:?} and {b:?} would both claim one herdr agent name"
            );
        }
        // Stable: relaunching the same task asks for the same name, so a task
        // taken down and started again is recognizable.
        assert_eq!(agent_name("foo.bar"), agent_name("foo.bar"));
    }

    #[test]
    fn a_launch_command_splits_into_a_binary_and_its_arguments() {
        assert_eq!(
            split_command("agy --dangerously-skip-permissions"),
            ("agy", vec!["--dangerously-skip-permissions"])
        );
        assert_eq!(split_command("claude"), ("claude", vec![]));
        assert_eq!(split_command(""), ("", vec![]));
        assert_eq!(
            split_command("  gemini   --yolo  "),
            ("gemini", vec!["--yolo"]),
            "runs of whitespace are one separator"
        );
    }

    /// herdr-tower supervises agy as gemini; herdr knows them apart, and the
    /// binary that will actually run is the more precise answer.
    #[test]
    fn the_backend_hears_agy_by_name_even_though_we_call_it_gemini() {
        assert_eq!(agent_kind("gemini", "agy"), "agy");
        assert_eq!(agent_kind("gemini", "gemini"), "gemini");
        assert_eq!(agent_kind("claude", "claude"), "claude");
    }

    /// The two halves have to agree: a gemini worker on an Antigravity machine
    /// is launched as `agy` with agy's flag, not gemini's.
    #[test]
    fn an_antigravity_machine_launches_agy_with_agys_flag() {
        let home = tempfile::tempdir().unwrap();
        let gemini = home.path().join(".gemini");
        std::fs::create_dir_all(gemini.join("antigravity-cli")).unwrap();
        let command = command_for("gemini", &gemini);
        let (bin, args) = split_command(&command);
        assert_eq!(agent_kind("gemini", bin), "agy");
        assert_eq!(args, vec!["--dangerously-skip-permissions"]);
    }

    /// The contract behind `tower_state::is_generated_worker_name`, which the row
    /// ordering uses to keep a machine-made name behind the task.
    ///
    /// It lives in `tower_state` because that is where labels are ranked and
    /// `tower_state` sits below this crate. This is the other end of it: every name
    /// this function produces must be recognizable there, or worker rows start
    /// showing a digest again.
    #[test]
    fn every_generated_name_is_recognizable_as_one() {
        for task in [
            "auth-api",
            "login-ui",
            "2단계",
            "a",
            "...",
            "Feat/GS-Synthetic",
            "averylongtasknamethatgoesonandonandon",
            "under_score",
        ] {
            let name = agent_name(task);
            assert!(
                tower_state::is_generated_worker_name(&name),
                "agent_name({task:?}) = {name:?} is not recognizable as generated"
            );
        }
    }

    /// The collision guard, against the bytes a server really sends.
    ///
    /// It read the name through a field declared as `name` while the wire key is
    /// `label`, so it answered "free" for every pane and never once fired — and
    /// a collision was then met by `start_agent` refusing after the branch, the
    /// worktree and the tab already existed, which is the debris the guard is
    /// there to prevent.
    #[test]
    fn a_name_another_pane_already_wears_is_taken() {
        let panes: Vec<herdrx::Pane> = serde_json::from_str(
            r#"[
                {"pane_id":"w2:p4C","agent":"claude","terminal_title_stripped":"2550 pg_stat_activity"},
                {"pane_id":"w2:p4S","agent":"claude","label":"herdr-rust"}
            ]"#,
        )
        .unwrap();

        assert!(any_pane_named(&panes, "herdr-rust"));
        assert!(!any_pane_named(&panes, "herdr-rus"), "not a prefix match");
        assert!(
            !any_pane_named(&panes, "2550 pg_stat_activity"),
            "a terminal title is not a name — it changes on its own every turn"
        );
        assert!(
            !any_pane_named(&[], "herdr-rust"),
            "nothing to collide with"
        );
    }

    /// A pane the tower named wears its ID in front of the name, and still
    /// takes the name.
    #[test]
    fn a_name_under_an_id_prefix_is_taken_too() {
        let panes: Vec<herdrx::Pane> = serde_json::from_str(
            r#"[{"pane_id":"w2:p4S","agent":"claude","label":"[w2:p4S] herdr-rust"}]"#,
        )
        .unwrap();
        assert!(any_pane_named(&panes, "herdr-rust"));
        assert!(!any_pane_named(&panes, "[w2:p4S] herdr-rust"));
    }

    /// A passing result, bound by a drive to the commit it was measured on.
    fn passed_at_head(state: &Path, task: &str) {
        let mut m = load_meta(state, task).unwrap();
        m.test_pass = Some(true);
        m.test_at = Some(now());
        m.drive = Some(crate::drive::Drive {
            state: crate::drive::DriveState::MergeReady,
            attempts: 1,
            failure: String::new(),
            head: git::head(&m.path).unwrap(),
            at: now(),
        });
        crate::meta::save_meta(state, &m).unwrap();
    }

    fn guide(state: &Path, task: &str) -> String {
        let mut buf = Vec::new();
        merge_guide(&mut buf, state, task, || false).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// 🚨 The command that merges says whether the pass is still about the
    /// branch. `test_pass` carries no commit, so a result from before the
    /// worker's last commit read exactly like a fresh one — here, of all
    /// places, where the answer decides what lands on the base.
    #[test]
    fn the_merge_guide_says_when_the_branch_has_moved_past_its_pass() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t5", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        std::fs::write(Path::new(&m.path).join("f.txt"), "tested\n").unwrap();
        commit_all(&m.path, "the tested commit");
        passed_at_head(state.path(), "t5");

        let fresh = guide(state.path(), "t5");
        assert!(fresh.contains("tests: ✔ passed"), "{fresh}");
        assert!(
            !fresh.contains("moved since"),
            "still the tested commit: {fresh}"
        );

        // The worker commits again after the verdict.
        std::fs::write(Path::new(&m.path).join("f.txt"), "untested\n").unwrap();
        commit_all(&m.path, "after the verdict");
        let moved = guide(state.path(), "t5");
        assert!(
            moved.contains("the branch has moved since; run wt test again"),
            "{moved}"
        );
    }

    /// Edge: a failure is reported as a failure whether or not the branch has
    /// moved — the warning is about trusting a pass, not about every result.
    #[test]
    fn a_failure_is_not_dressed_up_as_a_moved_pass() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t6", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        commit_all_with_file(&m.path, "one");
        passed_at_head(state.path(), "t6");
        let mut failed = load_meta(state.path(), "t6").unwrap();
        failed.test_pass = Some(false);
        crate::meta::save_meta(state.path(), &failed).unwrap();
        commit_all_with_file(&m.path, "two");

        let out = guide(state.path(), "t6");
        assert!(out.contains("tests: ✖ failed"), "{out}");
        assert!(!out.contains("moved since"), "{out}");
    }

    /// Uncommitted edits on the tested commit are not a moved branch. The pass
    /// still describes every commit the merge would take; what the merge would
    /// leave behind is named by the uncommitted-files warning instead.
    #[test]
    fn uncommitted_edits_are_not_reported_as_a_moved_branch() {
        let repo = fixture_repo();
        let state = tempfile::tempdir().unwrap();
        let m = new_task(
            state.path(),
            &opts("t7", repo.path().to_str().unwrap()),
            now(),
        )
        .unwrap();
        commit_all_with_file(&m.path, "tested");
        passed_at_head(state.path(), "t7");
        std::fs::write(Path::new(&m.path).join("f.txt"), "edited, not committed\n").unwrap();

        let out = guide(state.path(), "t7");
        assert!(out.contains("tests: ✔ passed"), "{out}");
        assert!(!out.contains("moved since"), "HEAD has not moved: {out}");
        assert!(
            out.contains("f.txt"),
            "the edit is named where it belongs: {out}"
        );
    }

    fn commit_all_with_file(path: &str, body: &str) {
        std::fs::write(Path::new(path).join("f.txt"), format!("{body}\n")).unwrap();
        commit_all(path, body);
    }

    /// A herdr whose `agent.start` answers `busy` times "not an available
    /// shell", then `last`. Returns the socket, how many starts it saw, and
    /// the params of each.
    #[cfg(unix)]
    #[allow(clippy::type_complexity)]
    fn starting_herdr(
        busy: u32,
        last: &'static str,
    ) -> (
        tempfile::TempDir,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
        std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        use std::io::{BufRead, BufReader};
        use std::sync::atomic::{AtomicU32, Ordering};
        let dir = tempfile::Builder::new()
            .prefix("ws")
            .tempdir_in("/tmp")
            .unwrap();
        let listener = std::os::unix::net::UnixListener::bind(dir.path().join("h.sock")).unwrap();
        let starts = std::sync::Arc::new(AtomicU32::new(0));
        let seen = starts.clone();
        let params = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sent = params.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut line = String::new();
                if BufReader::new(&conn).read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let req: serde_json::Value = serde_json::from_str(&line).unwrap();
                let id = req["id"].as_str().unwrap_or_default();
                sent.lock().unwrap().push(req["params"].clone());
                let body = if seen.fetch_add(1, Ordering::SeqCst) < busy {
                    r#"{"id":"$ID","error":{"code":"agent_pane_busy","message":"agent target pane w7:pBY is not an available shell"}}"#
                } else {
                    last
                };
                let _ = writeln!(conn, "{}", body.replace("$ID", id));
            }
        });
        (dir, starts, params)
    }

    #[cfg(unix)]
    #[test]
    fn the_start_waits_for_the_new_tabs_shell() {
        use std::sync::atomic::Ordering;
        let (dir, starts, _) = starting_herdr(2, r#"{"id":"$ID","result":{}}"#);
        let h = Herdr::at(dir.path().join("h.sock"));
        start_in(&h, "w7:pBY", "cs556", "claude", "claude").unwrap();
        assert_eq!(
            starts.load(Ordering::SeqCst),
            3,
            "two refusals, then the start"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_start_that_failed_for_another_reason_is_not_retried() {
        use std::sync::atomic::Ordering;
        let (dir, starts, _) = starting_herdr(
            0,
            r#"{"id":"$ID","error":{"code":"agent_not_ready","message":"blocked at startup"}}"#,
        );
        let h = Herdr::at(dir.path().join("h.sock"));
        let e = start_in(&h, "w7:pBY", "cs556", "claude", "claude").unwrap_err();
        assert!(e.is_code("agent_not_ready"), "{e}");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    /// Every retried start asks for the same agent: the name, kind, pane and
    /// arguments of the last attempt are those of the first.
    #[cfg(unix)]
    #[test]
    fn a_retried_start_sends_the_same_request_each_time() {
        use std::sync::atomic::Ordering;
        let (dir, starts, params) = starting_herdr(3, r#"{"id":"$ID","result":{}}"#);
        let h = Herdr::at(dir.path().join("h.sock"));
        start_in(
            &h,
            "w7:pBY",
            "Fix 556",
            "codex -s danger-full-access",
            "codex",
        )
        .unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 4);
        let params = params.lock().unwrap();
        assert!(params.windows(2).all(|w| w[0] == w[1]), "{params:?}");
        assert_eq!(params[0]["name"], agent_name("Fix 556").as_str());
        assert!(params[0]["name"].as_str().unwrap().starts_with("fix-556"));
        assert_eq!(params[0]["kind"], "codex");
        assert_eq!(params[0]["pane_id"], "w7:pBY");
        assert_eq!(
            params[0]["args"],
            serde_json::json!(["-s", "danger-full-access"])
        );
        assert!(
            params[0].get("timeout_ms").is_none(),
            "herdr's own readiness default"
        );
    }

    /// What reaches herdr is the launch command taken apart again: the
    /// binary named by kind (`agy` for that gemini), its arguments in order,
    /// and none at all for a bare binary.
    #[cfg(unix)]
    #[test]
    fn the_start_request_is_the_command_line_taken_apart() {
        let cases: &[(&str, &str, &str, &[&str])] = &[
            ("claude", "claude", "claude", &[]),
            (
                "claude",
                "claude --dangerously-skip-permissions",
                "claude",
                &["--dangerously-skip-permissions"],
            ),
            ("gemini", "agy --yolo", "agy", &["--yolo"]),
            ("gemini", "gemini --yolo", "gemini", &["--yolo"]),
            ("opencode", "opencode", "opencode", &[]),
        ];
        for (kind, command, want_kind, want_args) in cases {
            let (dir, _, params) = starting_herdr(0, r#"{"id":"$ID","result":{}}"#);
            let h = Herdr::at(dir.path().join("h.sock"));
            start_in(&h, "w1:p1", "t", command, kind).unwrap();
            let params = params.lock().unwrap();
            assert_eq!(params[0]["kind"], *want_kind, "{command}");
            match params[0].get("args") {
                None => assert!(want_args.is_empty(), "{command}"),
                Some(args) => assert_eq!(args, &serde_json::json!(want_args), "{command}"),
            }
            // Joined back, the binary and the arguments are the command.
            let (bin, args) = split_command(command);
            let mut back = vec![bin.to_string()];
            back.extend(args.iter().map(|a| a.to_string()));
            assert_eq!(back.join(" "), *command);
        }
    }

    /// For every number of refusals before the shell is up, within the retry
    /// budget, the start lands with exactly one call per refusal plus one.
    #[cfg(unix)]
    #[test]
    fn the_start_takes_one_call_per_refusal_plus_one() {
        use std::sync::atomic::Ordering;
        for busy in 0..4 {
            let (dir, starts, _) = starting_herdr(busy, r#"{"id":"$ID","result":{}}"#);
            let h = Herdr::at(dir.path().join("h.sock"));
            start_in(&h, "w7:pBY", "t", "claude", "claude").unwrap();
            assert_eq!(starts.load(Ordering::SeqCst), busy + 1, "{busy} refusals");
        }
    }
}
